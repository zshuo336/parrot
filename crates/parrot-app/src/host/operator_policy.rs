use super::DeployError;
use std::{io::Read, path::Path};

pub(super) const MAX_FILE_BYTES: usize = 1024 * 1024;

pub(super) async fn load_policy_file<T: Send + 'static>(
    path: impl AsRef<Path>,
    label: &'static str,
    parse: fn(&str) -> Result<T, DeployError>,
) -> Result<T, DeployError> {
    let path = path.as_ref().to_owned();
    tokio::task::spawn_blocking(move || {
        let invalid = |message: &str| DeployError::Config(format!("{label}: {message}"));
        if !std::fs::metadata(&path)
            .map_err(|error| invalid(&error.to_string()))?
            .is_file()
        {
            return Err(invalid("expected a regular file"));
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK);
        }
        let file = options
            .open(&path)
            .map_err(|error| invalid(&error.to_string()))?;
        let metadata = file
            .metadata()
            .map_err(|error| invalid(&error.to_string()))?;
        if !metadata.is_file() {
            return Err(invalid("expected a regular file"));
        }
        if metadata.len() > MAX_FILE_BYTES as u64 {
            return Err(invalid("file exceeds 1 MiB"));
        }
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| invalid(&error.to_string()))?;
        let text = std::str::from_utf8(&bytes).map_err(|_| invalid("expected UTF-8 TOML"))?;
        parse(text)
    })
    .await
    .map_err(|error| DeployError::Config(format!("{label} worker: {error}")))?
}
