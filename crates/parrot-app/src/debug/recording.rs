//! Owned recording thread. Transport producers only offer bounded memory copies.
use super::*;
use parrot_remote::capture::{CaptureLimits, CaptureReceiver, CaptureStats, FrameCapture};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[derive(Debug)]
pub struct RecordingReport {
    pub path: PathBuf,
    pub written: usize,
    pub stats: CaptureStats,
}
/// A single capture interval in a new file. Call `finish` to drain, sync and join.
/// Drop closes and joins, marking the log abandoned. Filesystem I/O and the
/// close barrier are blocking; use a dedicated thread or `spawn_blocking` from async code.
pub struct Recording {
    capture: FrameCapture,
    complete: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<Result<RecordingReport, ReplayError>>>,
}
impl Recording {
    pub fn start(
        path: &Path,
        capture_limits: CaptureLimits,
        limits: ReplayLimits,
    ) -> Result<Self, ReplayError> {
        limits.validate()?;
        let (capture, receiver) = FrameCapture::channel(capture_limits).map_err(io)?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path).map_err(io)?;
        file.try_lock().map_err(io)?;
        let mut sink = FileSink {
            file: LockedFile(file),
            path: path.into(),
            history: History::default(),
            limits,
            failed: false,
        };
        sink.envelope(&CaptureLine::Start(CaptureStart { version: 1 }))?;
        let complete = Arc::new(AtomicBool::new(false));
        let completion = complete.clone();
        let worker = std::thread::Builder::new()
            .name("parrot-recorder".into())
            .spawn(move || record(receiver, sink, completion))
            .map_err(io)?;
        Ok(Self {
            capture,
            complete,
            worker: Some(worker),
        })
    }
    pub fn capture(&self) -> FrameCapture {
        self.capture.clone()
    }
    pub fn finish(mut self) -> Result<RecordingReport, ReplayError> {
        self.complete.store(true, Ordering::Release);
        self.capture.close();
        self.join()
    }
    fn join(&mut self) -> Result<RecordingReport, ReplayError> {
        self.worker
            .take()
            .expect("recording worker owned")
            .join()
            .map_err(|_| io("recording worker panicked"))?
    }
}
impl Drop for Recording {
    fn drop(&mut self) {
        if self.worker.is_some() {
            self.capture.close();
            let _ = self.join();
        }
    }
}
impl FileSink {
    fn envelope(&mut self, envelope: &CaptureLine) -> Result<(), ReplayError> {
        if self.failed {
            return Err(ReplayError::Poisoned);
        }
        let size = line_len(envelope, self.limits)?;
        let bytes = self
            .history
            .bytes
            .checked_add(size)
            .filter(|n| *n <= self.limits.max_file_bytes)
            .ok_or_else(|| limit("file bytes"))?;
        let mut line = serde_json::to_vec(envelope).map_err(io)?;
        line.push(b'\n');
        self.file.write_all(&line).map_err(|error| {
            self.failed = true;
            io(error)
        })?;
        self.history.bytes = bytes;
        Ok(())
    }
}
fn record(
    mut receiver: CaptureReceiver,
    mut sink: FileSink,
    complete: Arc<AtomicBool>,
) -> Result<RecordingReport, ReplayError> {
    let result = (|| {
        let mut written = 0;
        while let Some(event) = receiver.blocking_recv() {
            if written == sink.limits.max_records {
                return Err(limit("record count"));
            }
            if event.frame.payload.len() > sink.limits.max_payload_bytes {
                return Err(limit("payload bytes"));
            }
            let mut record = record_sink(
                event.seq,
                event.frame.header.correlation_id,
                event.direction.as_str(),
                event.frame.header.frame_type,
                &event.frame.path,
                &event.frame.type_key,
                &event.frame.payload,
            );
            record.wire = Some(event.wire.clone());
            validate_record(&record, sink.limits)?;
            sequence(sink.history.last, record.seq)?;
            sink.history.last = Some(record.seq);
            sink.envelope(&CaptureLine::Frame(record))?;
            written += 1;
        }
        let stats = receiver.final_stats();
        let valid = complete.load(Ordering::Acquire)
            && stats.lost == 0
            && stats.queued == written as u64
            && stats.observed == stats.queued;
        sink.envelope(&CaptureLine::End(CaptureEnd {
            written,
            stats,
            complete: valid,
        }))?;
        sink.sync()?;
        if !valid {
            return Err(incomplete(format!("abandoned or lost frames: {stats:?}")));
        }
        Ok(RecordingReport {
            path: sink.path.clone(),
            written,
            stats,
        })
    })();
    // On I/O/limit failure stop producers too; retain incomplete evidence.
    receiver.close();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_write_error_closes_producers_and_preserves_incomplete_file() {
        let path = std::env::temp_dir().join(format!("parrot-recorder-io-{}", std::process::id()));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let mut file = file;
        file.write_all(b"{\"start\":{\"version\":1}}\n").unwrap();
        drop(file);
        // A real read-only descriptor deterministically rejects the worker's write.
        let file = std::fs::File::open(&path).unwrap();
        let sink = FileSink {
            file: LockedFile(file),
            path: path.clone(),
            history: History::default(),
            limits: Default::default(),
            failed: false,
        };
        let (capture, receiver) = FrameCapture::channel(Default::default()).unwrap();
        let conn = capture.connection("local", "mem").unwrap();
        conn.observe(
            parrot_remote::capture::CaptureDirection::Rx,
            &parrot_remote::frame::Frame::heartbeat(),
        );
        assert!(matches!(
            record(receiver, sink, Arc::new(AtomicBool::new(true))),
            Err(ReplayError::Io(_))
        ));
        let stats = capture.stats();
        conn.observe(
            parrot_remote::capture::CaptureDirection::Rx,
            &parrot_remote::frame::Frame::heartbeat(),
        );
        assert_eq!(capture.stats(), stats);
        assert!(ReplayLog::load_capture(&path).is_err());
        assert!(std::fs::read_to_string(&path).unwrap().contains("start"));
        std::fs::remove_file(path).unwrap();
    }
}
