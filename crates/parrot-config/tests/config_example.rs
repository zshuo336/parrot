use parrot_config::ParrotConfig;
use std::collections::BTreeSet;

fn keys(value: &toml::Value, prefix: &str, output: &mut BTreeSet<String>) {
    if matches!(
        prefix,
        "remote.receptionist.roles" | "remote.receptionist.relays"
    ) {
        output.insert(prefix.to_owned());
    } else if let Some(table) = value.as_table() {
        for (name, child) in table {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}.{name}")
            };
            keys(child, &path, output);
        }
    } else {
        output.insert(prefix.to_owned());
    }
}

#[test]
fn published_example_contains_every_documented_option_and_valid_data() {
    let source = include_str!("../../../parrot.toml.example");
    toml::from_str::<ParrotConfig>(source)
        .unwrap()
        .build()
        .unwrap();
    // Activate only commented TOML table headers and assignments. Prose stays
    // commented; the TOML parser checks the actual example syntax and types.
    let activated = source
        .lines()
        .map(|line| {
            let Some(comment) = line.trim_start().strip_prefix('#').map(str::trim_start) else {
                return line;
            };
            let table = comment.starts_with('[') && comment.ends_with(']');
            let assignment = comment.split_once('=').is_some_and(|(key, _)| {
                let key = key.trim();
                !key.is_empty() && key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
            });
            if table || assignment {
                comment
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let values: toml::Value = toml::from_str(&activated).unwrap();
    let mut actual = BTreeSet::new();
    keys(&values, "", &mut actual);
    assert_eq!(
        actual,
        ParrotConfig::documented()
            .keys()
            .map(|key| (*key).to_owned())
            .collect(),
        "the operator example must cover exactly the documented keys"
    );
    toml::from_str::<ParrotConfig>(&activated)
        .unwrap()
        .build()
        .unwrap();
}
