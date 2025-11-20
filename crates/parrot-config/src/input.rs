use std::{
    fs::OpenOptions,
    io::{self, Read},
    path::Path,
};

/// Maximum UTF-8 bytes in each raw or environment-expanded configuration.
pub const MAX_CONFIG_BYTES: usize = 1024 * 1024;

fn limit_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "configuration source and expanded text must each be at most 1048576 bytes",
    )
}

pub(super) fn read_source(path: &Path) -> io::Result<String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Check the opened descriptor without waiting for a FIFO writer or
        // taking ownership of a terminal. Symlink-based deployments still work.
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "configuration source must be a regular file",
        ));
    }
    if metadata.len() > MAX_CONFIG_BYTES as u64 {
        return Err(limit_error());
    }
    read_text(file)
}

fn read_text(reader: impl Read) -> io::Result<String> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_CONFIG_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    // Metadata is only an early rejection; growth cannot bypass the byte cap.
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(limit_error());
    }
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub(super) fn expand_env(raw: &str) -> io::Result<String> {
    expand_env_with(raw, |name| std::env::var(name).ok())
}

pub(super) fn expand_env_with(
    raw: &str,
    mut lookup: impl FnMut(&str) -> Option<String>,
) -> io::Result<String> {
    if raw.len() > MAX_CONFIG_BYTES {
        return Err(limit_error());
    }
    let mut out = String::with_capacity(raw.len());
    let mut remaining = raw;
    while let Some(start) = remaining.find("${") {
        append(&mut out, &remaining[..start])?;
        remaining = &remaining[start..];
        let Some(end) = remaining.find('}') else {
            break;
        };
        let inner = &remaining[2..end];
        let (name, default) = match inner.split_once(":-") {
            Some((name, default)) => (name, Some(default)),
            None => (inner, None),
        };
        let value = lookup(name).filter(|value| !value.is_empty());
        if let Some(value) = value.as_deref().or(default) {
            append(&mut out, value)?;
        }
        remaining = &remaining[end + 1..];
    }
    append(&mut out, remaining)?;
    Ok(out)
}

fn append(output: &mut String, text: &str) -> io::Result<()> {
    if text.len() > MAX_CONFIG_BYTES - output.len() {
        return Err(limit_error());
    }
    output.push_str(text);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_budget_does_not_depend_on_metadata_or_end_of_stream() {
        struct Source(usize);
        impl Read for Source {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.0 += buf.len();
                buf.fill(b' ');
                Ok(buf.len())
            }
        }
        let mut source = Source(0);
        let error = read_text(&mut source).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(source.0, MAX_CONFIG_BYTES + 1);
        assert_eq!(read_text(&b""[..]).unwrap(), "");
        assert_eq!(
            read_text(&[0xff][..]).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn expansion_counts_utf8_bytes_before_appending_and_stops_lookups() {
        let value = "x".repeat(MAX_CONFIG_BYTES - 2);
        assert_eq!(
            expand_env_with("${VALUE}ab", |_| Some(value.clone()))
                .unwrap()
                .len(),
            MAX_CONFIG_BYTES
        );
        for raw in ["${VALUE}abc", "abc${VALUE}", "${VALUE}${VALUE}"] {
            assert_eq!(
                expand_env_with(raw, |_| Some(value.clone()))
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
        let value = "\u{00e9}".repeat(MAX_CONFIG_BYTES / 2);
        assert_eq!(
            expand_env_with("${VALUE}", |_| Some(value.clone()))
                .unwrap()
                .len(),
            MAX_CONFIG_BYTES
        );
        assert!(expand_env_with("${VALUE}x", |_| Some(value.clone())).is_err());
        let mut lookups = 0;
        let result = expand_env_with("${A}${B}", |_| {
            lookups += 1;
            Some("x".repeat(MAX_CONFIG_BYTES + 1))
        });
        assert!(result.is_err());
        assert_eq!(lookups, 1);
    }
}
