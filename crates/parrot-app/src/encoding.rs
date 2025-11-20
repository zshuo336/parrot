//! Bounded TOML output without document table-path expansion.
use crate::limits::{add, AdmissionError};
use std::fmt::{self, Write};

struct Buffer {
    text: String,
    bytes: u64,
    limit: u64,
    resource: &'static str,
    failure: Option<AdmissionError>,
}
impl Buffer {
    fn new(limit: u64, resource: &'static str) -> Self {
        Self {
            text: String::new(),
            bytes: 0,
            limit,
            resource,
            failure: None,
        }
    }
    fn error(&self) -> String {
        self.failure
            .as_ref()
            .map_or_else(|| "TOML formatting failed".into(), ToString::to_string)
    }
}
impl Write for Buffer {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let result = add(
            self.resource,
            &mut self.bytes,
            text.len() as u64,
            self.limit,
        )
        .and_then(|()| {
            self.text
                .try_reserve(text.len())
                .map_err(|_| AdmissionError::Allocation("encoded TOML bytes"))
        });
        if let Err(error) = result {
            self.failure = Some(error);
            return Err(fmt::Error);
        }
        self.text.push_str(text);
        Ok(())
    }
}

// Callers admit the model before entering the recursive serde conversion.
pub(crate) fn canonical(
    value: &impl serde::Serialize,
    limit: u64,
) -> Result<String, toml::ser::Error> {
    use serde::ser::Error;
    use toml_edit::visit_mut::VisitMut;
    let mut document = toml_edit::ser::to_document(value).map_err(toml::ser::Error::custom)?;
    CanonicalDocument { in_value: false }.visit_document_mut(&mut document);
    let mut buffer = Buffer::new(limit, "source bytes");
    if document_table(document.as_table(), &mut Vec::new(), false, &mut buffer).is_err() {
        return Err(toml::ser::Error::custom(buffer.error()));
    }
    Ok(buffer.text)
}

// Match toml 0.8's default DocumentFormatter using the library's conversions.
// Conversion stops at values: mixed arrays must retain their inline tables.
struct CanonicalDocument {
    in_value: bool,
}
impl toml_edit::visit_mut::VisitMut for CanonicalDocument {
    fn visit_item_mut(&mut self, item: &mut toml_edit::Item) {
        let parent = self.in_value;
        if !parent {
            let converted = std::mem::take(item)
                .into_table()
                .map(toml_edit::Item::Table)
                .unwrap_or_else(|item| item);
            let converted = converted
                .into_array_of_tables()
                .map(toml_edit::Item::ArrayOfTables)
                .unwrap_or_else(|item| item);
            self.in_value = converted.is_value();
            *item = converted;
        }
        toml_edit::visit_mut::visit_item_mut(self, item);
        self.in_value = parent;
    }
    fn visit_table_mut(&mut self, table: &mut toml_edit::Table) {
        table.decor_mut().clear();
        if !table.is_empty() {
            table.set_implicit(true);
        }
        toml_edit::visit_mut::visit_table_mut(self, table);
    }
    fn visit_value_mut(&mut self, value: &mut toml_edit::Value) {
        value.decor_mut().clear();
        toml_edit::visit_mut::visit_value_mut(self, value);
    }
}

// Only serde-created documents enter here: no comments, dotted keys, or source
// positions. Preserve toml::to_string ordering without DocumentMut's
// collection of a cloned full key path for every table before the first write.
fn document_table<'a>(
    table: &'a toml_edit::Table,
    path: &mut Vec<&'a toml_edit::Key>,
    array: bool,
    buffer: &mut Buffer,
) -> fmt::Result {
    let visible = array || !table.is_implicit() || table.iter().any(|(_, item)| item.is_value());
    if !path.is_empty() && visible {
        if !buffer.text.is_empty() {
            buffer.write_char('\n')?;
        }
        buffer.write_str(if array { "[[" } else { "[" })?;
        for (index, key) in path.iter().enumerate() {
            if index != 0 {
                buffer.write_char('.')?;
            }
            write!(buffer, "{key}")?;
        }
        buffer.write_str(if array { "]]\n" } else { "]\n" })?;
    }
    for (key, item) in table.iter() {
        if let toml_edit::Item::Value(value) = item {
            let key = table.key(key).expect("iterated table key exists");
            writeln!(buffer, "{key} = {value}")?;
        }
    }
    for (key, item) in table.iter() {
        match item {
            toml_edit::Item::Table(child) => {
                path.push(table.key(key).expect("iterated table key exists"));
                document_table(child, path, false, buffer)?;
                path.pop();
            }
            toml_edit::Item::ArrayOfTables(children) => {
                path.push(table.key(key).expect("iterated table key exists"));
                for child in children {
                    document_table(child, path, true, buffer)?;
                }
                path.pop();
            }
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn table(table: &toml::Table, remaining: &mut u64) -> Result<String, String> {
    table_with_limit(table, remaining, *remaining, "encoded TOML bytes")
}

pub(crate) fn runtime_env(
    table: &toml::Table,
    remaining: &mut u64,
    source_limit: u64,
) -> Result<String, String> {
    let (limit, resource) = if source_limit < *remaining {
        (source_limit, "source bytes")
    } else {
        (*remaining, "encoded TOML bytes")
    };
    table_with_limit(table, remaining, limit, resource)
}

fn table_with_limit(
    table: &toml::Table,
    remaining: &mut u64,
    limit: u64,
    resource: &'static str,
) -> Result<String, String> {
    let mut buffer = Buffer::new(limit, resource);
    for (key, value) in table {
        // ValueSerializer keeps nested tables inline; DocumentMut repeats parent
        // paths for every nested table before its first output write.
        let key = toml_edit::Key::new(key);
        let value = serde::Serialize::serialize(value, toml_edit::ser::ValueSerializer::new())
            .map_err(|error| format!("TOML encoding failed: {error}"))?;
        if writeln!(buffer, "{key} = {value}").is_err() {
            return Err(buffer.error());
        }
    }
    *remaining -= buffer.bytes;
    Ok(buffer.text)
}

// Owned conversion can reject a programmatically constructed deeply nested value.
// Retire it iteratively so returning that error does not recursively drop it.
pub(crate) fn drop_value(value: toml::Value) {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            toml::Value::Array(values) => pending.extend(values),
            toml::Value::Table(table) => pending.extend(table.into_iter().map(|(_, value)| value)),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn exact_utf8_output_boundary_and_shared_remaining() {
        let table = toml::Table::from_iter([("x".into(), toml::Value::String("中".into()))]);
        let expected = "x = \"中\"\n";
        let mut remaining = expected.len() as u64;
        assert_eq!(super::table(&table, &mut remaining).unwrap(), expected);
        assert_eq!(remaining, 0);
        assert!(super::table(&table, &mut remaining)
            .unwrap_err()
            .contains("encoded TOML bytes budget exceeded"));
        let mut remaining = expected.len() as u64 - 1;
        assert!(super::table(&table, &mut remaining).is_err());
    }

    #[test]
    fn sibling_tables_do_not_repeat_the_parent_key() {
        let key = "a".repeat(8192);
        let children = (0..1000)
            .map(|i| (format!("k{i}"), toml::Value::Table(toml::Table::new())))
            .collect();
        let table = toml::Table::from_iter([(key.clone(), toml::Value::Table(children))]);
        let text = super::table(&table, &mut 32_768).unwrap();
        assert_eq!(text.matches(&key).count(), 1);
        assert!(text.len() < 32_768);
        assert_eq!(toml::from_str::<toml::Table>(&text).unwrap(), table);
    }

    #[test]
    fn aggregate_documents_share_the_output_budget() {
        let table = toml::Table::from_iter([("x".into(), toml::Value::Integer(1))]);
        let mut remaining = 11;
        assert_eq!(super::table(&table, &mut remaining).unwrap(), "x = 1\n");
        assert_eq!(remaining, 5);
        assert!(super::table(&table, &mut remaining)
            .unwrap_err()
            .contains("encoded TOML bytes budget exceeded"));
    }
}
