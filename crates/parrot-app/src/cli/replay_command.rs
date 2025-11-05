use super::Error;
use crate::{
    assemble::AssemblingContext,
    debug::{ReplayError, ReplayLog, ReplayOptions, ReplayReport},
};
use parrot_api::types::BoxedActorRef;
use parrot_remote::LocalLookup;
use serde::Serialize;
use std::{fs::File, io::Write, path::PathBuf, sync::Arc};

const MAX_REPORT_BYTES: usize = 256 * 1024 * 1024;

pub(super) struct ReplayCommand {
    log: ReplayLog,
    options: ReplayOptions,
    report: File,
}
pub(super) struct Execution<'a> {
    pub command: Option<&'a ReplayCommand>,
    pub result: &'a mut Option<Result<ReplayReport, ReplayError>>,
}
impl ReplayCommand {
    pub async fn prepare(
        input: PathBuf,
        output: PathBuf,
        options: ReplayOptions,
        legacy: bool,
    ) -> Result<Self, Error> {
        // Validate the entire input before starting host resources or reserving
        // the report. The descriptor and allocation limits are enforced by the
        // same library loader used for normal recordings.
        let limits = options.limits;
        let (log, report) = tokio::task::spawn_blocking(move || -> Result<_, Error> {
            let log = if legacy {
                ReplayLog::load_with_limits(&input, limits)?
            } else {
                ReplayLog::load_capture_with_limits(&input, limits)?
            };
            let mut open = std::fs::OpenOptions::new();
            open.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                open.mode(0o600);
            }
            let report = open.open(output)?;
            Ok((log, report))
        })
        .await??;
        Ok(Self {
            log,
            options,
            report,
        })
    }
    pub async fn replay(
        &self,
        context: &AssemblingContext,
        thread: &Arc<parrot::thread::ThreadActorSystem>,
    ) -> Result<ReplayReport, ReplayError> {
        let seq = self.log.all().first().map_or(0, |r| r.seq);
        let mut lookup = Lookup {
            actors: Vec::new(),
            thread: thread.clone(),
        };
        for name in context.started() {
            if let (Some(paths), Some(actors)) =
                (context.component_paths(name), context.component_refs(name))
            {
                for (path, actor) in paths.iter().zip(actors) {
                    let actor = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        actor.clone_boxed()
                    }))
                    .map_err(|panic| {
                        crate::disposal::drop_catching(panic);
                        ReplayError::Delivery {
                            seq,
                            completed: 0,
                            reason: "replay reference cloning panicked".into(),
                        }
                    })?;
                    lookup.actors.push((path.clone(), Some(actor)));
                }
            }
        }
        let result = self.log.replay_local(&lookup, self.options.clone()).await;
        let released = lookup.release();
        match result {
            Ok(report) if !released => Err(ReplayError::Delivery {
                seq: self.log.all().last().map_or(0, |r| r.seq),
                completed: report.delivered,
                reason: "replay reference disposal panicked".into(),
            }),
            other => other,
        }
    }
    /// Publish the replay result only after host teardown has completed. A
    /// teardown failure keeps successful delivery details but marks failure.
    pub async fn finish(
        self,
        result: Option<Result<ReplayReport, ReplayError>>,
        host: &Result<(), Error>,
    ) -> Result<(), Error> {
        let error = host.as_ref().err().map(ToString::to_string);
        let report = match result {
            Some(Ok(report)) => Report {
                version: 1,
                status: if error.is_none() {
                    "succeeded"
                } else {
                    "failed"
                },
                direction: direction(&self.options),
                tell_mode: tell_mode(&self.options),
                delivered: Some(report.delivered),
                replay_completed: true,
                failure_seq: None,
                replies: Some(report.replies),
                error,
            },
            Some(Err(failure)) => {
                let (delivered, failure_seq) = match failure {
                    ReplayError::Delivery { seq, completed, .. } => (Some(completed), Some(seq)),
                    _ => (None, None),
                };
                Report {
                    version: 1,
                    status: "failed",
                    direction: direction(&self.options),
                    tell_mode: tell_mode(&self.options),
                    delivered,
                    replay_completed: false,
                    failure_seq,
                    replies: None,
                    error,
                }
            }
            None => Report {
                version: 1,
                status: "failed",
                direction: direction(&self.options),
                tell_mode: tell_mode(&self.options),
                delivered: None,
                replay_completed: false,
                failure_seq: None,
                replies: None,
                error,
            },
        };
        tokio::task::spawn_blocking(move || -> Result<(), Error> {
            let mut output = Bounded {
                file: self.report,
                written: 0,
            };
            serde_json::to_writer(&mut output, &report)?;
            output.write_all(b"\n")?;
            output.file.sync_data()?;
            Ok(())
        })
        .await??;
        Ok(())
    }
}
fn direction(options: &ReplayOptions) -> &'static str {
    match options.direction {
        crate::debug::ReplayDirection::Rx => "rx",
        crate::debug::ReplayDirection::Tx => "tx",
    }
}
fn tell_mode(options: &ReplayOptions) -> &'static str {
    match options.tell_mode {
        crate::debug::ReplayTellMode::Admission => "admission",
        crate::debug::ReplayTellMode::Completion => "completion",
    }
}
struct Lookup {
    actors: Vec<(String, Option<BoxedActorRef>)>,
    thread: Arc<parrot::thread::ThreadActorSystem>,
}
impl Lookup {
    fn release(&mut self) -> bool {
        let mut ok = true;
        for (_, actor) in &mut self.actors {
            ok &= crate::disposal::drop_catching(actor.take());
        }
        ok
    }
}
impl Drop for Lookup {
    fn drop(&mut self) {
        self.release();
    }
}
#[async_trait::async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        // Gateway entries are exact assembled references. Local descendants
        // resolve only in the fresh host's own engine, never a remote URI.
        if let Some((_, Some(actor))) = self.actors.iter().find(|(p, _)| p == path) {
            return Some(actor.clone_boxed());
        }
        crate::host::HostLookup {
            ts: self.thread.clone(),
        }
        .lookup(path)
        .await
    }
}
#[derive(Serialize)]
struct Report {
    version: u8,
    status: &'static str,
    direction: &'static str,
    tell_mode: &'static str,
    /// Confirmed completed ASK/completion TELL calls and admitted ordinary TELLs.
    /// None means progress was not available (e.g. cancellation during replay).
    delivered: Option<usize>,
    replay_completed: bool,
    failure_seq: Option<u64>,
    #[serde(serialize_with = "serialize_replies")]
    replies: Option<Vec<crate::debug::ReplayReply>>,
    error: Option<String>,
}
fn serialize_replies<S: serde::Serializer>(
    replies: &Option<Vec<crate::debug::ReplayReply>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeSeq;
    let Some(replies) = replies else {
        return serializer.serialize_none();
    };
    #[derive(Serialize)]
    struct Reply<'a> {
        seq: u64,
        cid: u64,
        batch_index: Option<usize>,
        type_key: &'a str,
        payload_hex: String,
    }
    let mut seq = serializer.serialize_seq(Some(replies.len()))?;
    for reply in replies {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut hex = String::with_capacity(reply.payload.len() * 2);
        for byte in &reply.payload {
            hex.push(DIGITS[(byte >> 4) as usize] as char);
            hex.push(DIGITS[(byte & 15) as usize] as char);
        }
        seq.serialize_element(&Reply {
            seq: reply.seq,
            cid: reply.cid,
            batch_index: reply.batch_index,
            type_key: &reply.type_key,
            payload_hex: hex,
        })?;
    }
    seq.end()
}
struct Bounded {
    file: File,
    written: usize,
}
impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_REPORT_BYTES.saturating_sub(self.written) {
            return Err(std::io::Error::other(
                "replay report exceeds 256 MiB; retained incomplete output",
            ));
        }
        let n = self.file.write(bytes)?;
        self.written += n;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}
