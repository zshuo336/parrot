//! Bounded local history queries. The socket is created only in a private directory.
use super::Error;
use crate::debug::{TraceHistory, MAX_SERVICE_NAME_BYTES};
use parrot_api::trace::SpanForest;
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::watch,
    task::JoinHandle,
};

const TEXT_LIMIT: usize = 1024 * 1024;
const RESPONSE_LIMIT: usize = TEXT_LIMIT * 2 + 4096;
const REQUEST_LIMIT: usize = 4096;
const IO_BUDGET: Duration = Duration::from_secs(5);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u8,
    app: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    version: u8,
    app: String,
    recorder: String,
    completed: u64,
    history_evicted: u64,
    counters_overflowed: bool,
    terminal: bool,
    text: String,
}

fn valid_app(app: &str) -> bool {
    !app.is_empty() && app.len() <= MAX_SERVICE_NAME_BYTES && !app.chars().any(char::is_control)
}

fn interrupted(follow: bool) -> Result<(), Error> {
    if follow {
        Ok(())
    } else {
        Err("trace query interrupted".into())
    }
}

fn decode_object<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, Error> {
    struct Object<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for Object<T> {
        type Value = T;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a trace query JSON object")
        }

        fn visit_map<M: serde::de::MapAccess<'de>>(self, map: M) -> Result<T, M::Error> {
            T::deserialize(serde::de::value::MapAccessDeserializer::new(map))
        }
    }
    // Keep the typed decoder's duplicate-field checks; a Value projection loses them.
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let object =
        serde::de::Deserializer::deserialize_map(&mut decoder, Object(std::marker::PhantomData))?;
    decoder.end()?;
    Ok(object)
}

pub(super) fn validate_path(path: &Path) -> Result<PathBuf, Error> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let metadata = std::fs::symlink_metadata(parent)?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(
            "trace socket parent must be a private directory owned by this user (mode 0700)".into(),
        );
    }
    let name = path.file_name().ok_or("trace socket requires a filename")?;
    let path = parent.canonicalize()?.join(name);
    if std::fs::symlink_metadata(&path).is_ok() {
        return Err("trace socket path already exists; it will not be replaced".into());
    }
    Ok(path)
}

struct SocketPath {
    path: PathBuf,
    device: u64,
    inode: u64,
}
impl Drop for SocketPath {
    fn drop(&mut self) {
        if let Ok(metadata) = std::fs::symlink_metadata(&self.path) {
            if metadata.file_type().is_socket()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode
            {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

pub(super) struct Server {
    stop: watch::Sender<bool>,
    ended: watch::Receiver<bool>,
    task: JoinHandle<Result<(), Error>>,
}
impl Server {
    pub(super) fn start(path: &Path, app: String, history: TraceHistory) -> Result<Self, Error> {
        if !valid_app(&app) {
            return Err("trace query application name is invalid".into());
        }
        let path = validate_path(path)?;
        let listener = std::os::unix::net::UnixListener::bind(&path)?;
        let metadata = std::fs::symlink_metadata(&path)?;
        let owned_path = SocketPath {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        std::fs::set_permissions(&owned_path.path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let listener = UnixListener::from_std(listener)?;
        let (stop, mut stopping) = watch::channel(false);
        let (end, ended) = watch::channel(false);
        let task = tokio::spawn(async move {
            let _owned_path = owned_path;
            let result = async {
                loop {
                    let (mut stream, _) = tokio::select! {
                        biased;
                        _ = stopping.changed() => break,
                        accepted = listener.accept() => accepted?,
                    };
                    // A single client owns bounded work. Slow or malformed clients
                    // cannot accumulate per-client tasks or serialization owners.
                    let request = tokio::select! {
                        biased;
                        _ = stopping.changed() => break,
                        result = tokio::time::timeout(IO_BUDGET, read_frame(&mut stream, REQUEST_LIMIT)) => result,
                    };
                    let Ok(Ok(request)) = request else { continue; };
                    let Ok(request) = decode_object::<Request>(&request) else { continue; };
                    if request.version != 1 || request.app != app { continue; }
                    let view = history.clone();
                    let application = app.clone();
                    // Always join finite serialization, including during shutdown.
                    let response = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, Error> {
                        let view = view.snapshot();
                        let rendered = SpanForest::new(&view.snapshot)?.render_text(TEXT_LIMIT)?;
                        let response = Response {
                            version: 1, app: application,
                            recorder: view.snapshot.recorder_id.iter().map(|b| format!("{b:02x}")).collect(),
                            completed: view.snapshot.counts.completed,
                            history_evicted: view.evicted,
                            counters_overflowed: view.counters_overflowed || view.snapshot.counts.counters_overflowed,
                            terminal: view.snapshot.terminal, text: rendered.text,
                        };
                        let bytes = serde_json::to_vec(&response)?;
                        if bytes.len() > RESPONSE_LIMIT { return Err("trace query response exceeds byte budget".into()); }
                        Ok(bytes)
                    }).await??;
                    tokio::select! {
                        biased;
                        _ = stopping.changed() => break,
                        _ = tokio::time::timeout(IO_BUDGET, write_frame(&mut stream, &response)) => {},
                    }
                }
                Ok::<(), Error>(())
            }.await;
            end.send_replace(true);
            result
        });
        Ok(Self { stop, ended, task })
    }

    pub(super) async fn failed(&self) -> Error {
        let mut ended = self.ended.clone();
        while !*ended.borrow_and_update() {
            if ended.changed().await.is_err() {
                break;
            }
        }
        "application trace query service stopped before host shutdown".into()
    }

    pub(super) async fn finish(self) -> Result<(), Error> {
        self.stop.send_replace(true);
        self.task.await?
    }
}

async fn read_frame(stream: &mut UnixStream, limit: usize) -> Result<Vec<u8>, Error> {
    let length = stream.read_u32().await? as usize;
    if length == 0 || length > limit {
        return Err("trace query frame exceeds byte budget".into());
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    Ok(bytes)
}
async fn write_frame(stream: &mut UnixStream, bytes: &[u8]) -> Result<(), Error> {
    stream.write_u32(bytes.len().try_into()?).await?;
    stream.write_all(bytes).await?;
    Ok(())
}

async fn query(path: &Path, app: &str) -> Result<Response, Error> {
    tokio::time::timeout(IO_BUDGET, async {
        let mut stream = UnixStream::connect(path).await?;
        write_frame(
            &mut stream,
            &serde_json::to_vec(&Request {
                version: 1,
                app: app.into(),
            })?,
        )
        .await?;
        let response: Response = decode_object(&read_frame(&mut stream, RESPONSE_LIMIT).await?)?;
        if response.version != 1
            || response.app != app
            || response.recorder.len() != 32
            || !response.recorder.bytes().all(|b| b.is_ascii_hexdigit())
            || response.text.len() > TEXT_LIMIT
            || response
                .text
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t')
        {
            return Err::<Response, Error>("invalid trace query response".into());
        }
        Ok(response)
    })
    .await
    .map_err(|_| "trace query exceeded request deadline")?
}

pub(super) async fn execute(arguments: impl IntoIterator<Item = OsString>) -> Result<(), Error> {
    let mut args = arguments.into_iter();
    let app = args
        .next()
        .ok_or("trace requires an application name")?
        .into_string()
        .map_err(|_| "application name must be UTF-8")?;
    if !valid_app(&app) {
        return Err("invalid trace application name".into());
    }
    let mut socket = None;
    let mut follow = false;
    let mut interval = None;
    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("--socket") if socket.is_none() => {
                socket = Some(PathBuf::from(
                    args.next().ok_or("--socket requires a path")?,
                ))
            }
            Some("--follow") if !follow => follow = true,
            Some("--interval-ms") if interval.is_none() => {
                let value = args.next().ok_or("--interval-ms requires a value")?;
                let value: u64 = value
                    .to_str()
                    .ok_or("invalid query interval")?
                    .parse()
                    .map_err(|_| "invalid query interval")?;
                if !(100..=60000).contains(&value) {
                    return Err("query interval must be in 100..=60000 milliseconds".into());
                }
                interval = Some(value);
            }
            _ => return Err("unknown or duplicate trace argument".into()),
        }
    }
    if interval.is_some() && !follow {
        return Err("--interval-ms requires --follow".into());
    }
    let socket = socket.ok_or("trace requires --socket <path>")?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut last = None;
    loop {
        let response = tokio::select! {
            biased;
            _ = terminate.recv() => return interrupted(follow),
            _ = interrupt.recv() => return interrupted(follow),
            response = query(&socket, &app) => response?,
        };
        if last.as_ref()
            != Some(&(
                response.text.clone(),
                response.history_evicted,
                response.counters_overflowed,
            ))
        {
            use std::io::Write;
            let mut output = std::io::stdout().lock();
            writeln!(
                output,
                "application={app:?} history_evicted={} history_counters_overflowed={}",
                response.history_evicted, response.counters_overflowed
            )?;
            write!(output, "{}", response.text)?;
            output.flush()?;
            last = Some((
                response.text,
                response.history_evicted,
                response.counters_overflowed,
            ));
        }
        if !follow || response.terminal {
            return Ok(());
        }
        tokio::select! {
            biased;
            _ = terminate.recv() => return Ok(()),
            _ = interrupt.recv() => return Ok(()),
            _ = tokio::time::sleep(Duration::from_millis(interval.unwrap_or(250))) => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_decoder_rejects_duplicate_request_fields_and_preserves_object_contract() {
        for request in [
            &br#"{"version":1,"version":1,"app":"orders"}"#[..],
            &br#"{"version":1,"app":"other","app":"orders"}"#[..],
            &br#"{"version":1,"app":"orders","a\u0070p":"orders"}"#[..],
            &br#"{"version":1,"app":"orders"} {}"#[..],
            &br#"[1,"orders"]"#[..],
            &br#"null"#[..],
            &br#"{"version":1,"app":"orders","unexpected":true}"#[..],
        ] {
            assert!(
                decode_object::<Request>(request).is_err(),
                "accepted ambiguous or non-object request: {:?}",
                String::from_utf8_lossy(request)
            );
        }
        let request: Request =
            decode_object(br#" { "a\u0070p": "orders", "version": 1 } "#).unwrap();
        assert_eq!(request.version, 1);
        assert_eq!(request.app, "orders");
    }

    #[tokio::test]
    async fn client_rejects_duplicate_response_fields_before_printing() {
        let response = serde_json::to_string(&Response {
            version: 1,
            app: "orders".into(),
            recorder: "0".repeat(32),
            completed: 1,
            history_evicted: 0,
            counters_overflowed: false,
            terminal: false,
            text: "work\n".into(),
        })
        .unwrap();
        for (key, first) in [
            (r#""version""#, "1"),
            (r#""app""#, r#""other""#),
            (r#""a\u0070p""#, r#""orders""#),
            (r#""recorder""#, r#""bad""#),
            (r#""completed""#, "0"),
            (r#""history_evicted""#, "2"),
            (r#""counters_overflowed""#, "true"),
            (r#""terminal""#, "true"),
            (r#""text""#, r#""different""#),
        ] {
            let duplicate = format!("{{{key}:{first},{}", response.strip_prefix('{').unwrap());
            assert!(
                fake_response(duplicate.into_bytes(), None).await.is_err(),
                "accepted duplicate response field {key}"
            );
        }
        let parsed = fake_response(response.into_bytes(), None).await.unwrap();
        assert_eq!(parsed.text, "work\n");
        assert_eq!(parsed.completed, 1);
    }

    async fn fake_response(
        bytes: Vec<u8>,
        declared_length: Option<u32>,
    ) -> Result<Response, Error> {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("query.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_frame(&mut stream, REQUEST_LIMIT).await.unwrap();
            stream
                .write_u32(declared_length.unwrap_or(bytes.len() as u32))
                .await
                .unwrap();
            stream.write_all(&bytes).await.unwrap();
        });
        let result = query(&path, "orders").await;
        server.await.unwrap();
        result
    }

    #[tokio::test]
    async fn client_checks_response_identity_text_and_wire_bounds() {
        let valid = serde_json::json!({
            "version": 1, "app": "orders", "recorder": "0123456789abcdef0123456789abcdef",
            "completed": 1, "history_evicted": 0, "counters_overflowed": false,
            "terminal": false, "text": "target=\"real-work\"\n",
        });
        let result = fake_response(serde_json::to_vec(&valid).unwrap(), None)
            .await
            .unwrap();
        assert_eq!(result.text, "target=\"real-work\"\n");
        for (field, value) in [
            ("version", serde_json::json!(2)),
            ("app", serde_json::json!("other")),
            ("recorder", serde_json::json!("x".repeat(32))),
            ("recorder", serde_json::json!("0".repeat(31))),
            ("text", serde_json::json!("terminal\u{1b}[2J")),
            ("text", serde_json::json!("x".repeat(TEXT_LIMIT + 1))),
            ("unknown", serde_json::json!(true)),
        ] {
            let mut response = valid.clone();
            response[field] = value;
            assert!(
                fake_response(serde_json::to_vec(&response).unwrap(), None)
                    .await
                    .is_err(),
                "accepted invalid field {field}"
            );
        }
        for length in [0, u32::MAX] {
            assert!(fake_response(Vec::new(), Some(length)).await.is_err());
        }
        assert!(fake_response(b"{".to_vec(), None).await.is_err());
        assert!(fake_response(b"{".to_vec(), Some(100)).await.is_err());
        let positional = serde_json::json!([
            1,
            "orders",
            "0123456789abcdef0123456789abcdef",
            1,
            0,
            false,
            false,
            "real-work\n"
        ]);
        assert!(
            fake_response(serde_json::to_vec(&positional).unwrap(), None)
                .await
                .is_err(),
            "positional array is not a protocol object"
        );
    }

    #[tokio::test]
    async fn query_deadline_includes_stalled_response_body() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("query.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (started, received) = tokio::sync::oneshot::channel();
        let (finish, finished) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_frame(&mut stream, REQUEST_LIMIT).await.unwrap();
            stream.write_u32(100).await.unwrap();
            stream.write_all(b"{").await.unwrap();
            started.send(()).unwrap();
            finished.await.unwrap();
        });
        let reader = tokio::spawn(async move { query(&path, "orders").await });
        received.await.unwrap();
        tokio::time::pause();
        tokio::time::advance(IO_BUDGET + Duration::from_millis(1)).await;
        assert_eq!(
            reader.await.unwrap().err().unwrap().to_string(),
            "trace query exceeded request deadline"
        );
        finish.send(()).unwrap();
        server.await.unwrap();
    }
}
