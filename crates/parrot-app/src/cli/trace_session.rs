//! CLI ownership of optional startup/request recording and continuous export.
use super::Error;
use crate::debug::{TraceExportConfig, TraceExportEnd, TraceExporter};
use parrot_api::trace::{SpanGuard, SpanKind, SpanStatus, TraceContext};
use parrot_remote::{IncomingTracePolicy, RemoteActorSystem, TraceRecorderAttachment};
use std::future::Future;
use std::time::Duration;

fn record_startup<T>(
    root: TraceContext,
    startup: impl Future<Output = Result<T, Error>>,
) -> impl Future<Output = Result<T, Error>> {
    parrot_api::trace::scope(Some(root), async move {
        let recording = SpanGuard::current(SpanKind::Application, "/application/startup");
        let result = startup.await;
        recording.finish(if result.is_ok() {
            SpanStatus::Ok
        } else {
            SpanStatus::Error
        });
        result
    })
}

pub(super) struct Session {
    exporter: TraceExporter,
    attachment: TraceRecorderAttachment,
    #[cfg(unix)]
    query: Option<super::trace_query::Server>,
}
impl Session {
    pub(super) async fn start(
        config: TraceExportConfig,
        remote: &RemoteActorSystem,
        socket: Option<&std::path::Path>,
    ) -> Result<Self, Error> {
        let name = config.service_name.clone();
        let (mut exporter, history) = if socket.is_some() {
            let (exporter, history) = TraceExporter::start_with_history(config)?;
            (exporter, Some(history))
        } else {
            (TraceExporter::start(config)?, None)
        };
        let attachment =
            exporter.attach_remote_with_policy(remote, IncomingTracePolicy::StartRoots)?;
        #[cfg(unix)]
        let query = match socket.zip(history) {
            Some((path, history)) => match super::trace_query::Server::start(path, name, history) {
                Ok(server) => Some(server),
                Err(error) => {
                    drop(attachment);
                    exporter.shutdown(Duration::ZERO).await?;
                    return Err(error);
                }
            },
            None => None,
        };
        #[cfg(not(unix))]
        let _ = (name, history);
        Ok(Self {
            exporter,
            attachment,
            #[cfg(unix)]
            query,
        })
    }
    pub(super) async fn failed(&self) -> Error {
        #[cfg(unix)]
        if let Some(query) = &self.query {
            return tokio::select! {
                status = self.exporter.wait_until_stopped() => format!(
                    "application trace exporter stopped before host shutdown: {:?}", status.end
                ).into(),
                error = query.failed() => error,
            };
        }
        let status = self.exporter.wait_until_stopped().await;
        format!(
            "application trace exporter stopped before host shutdown: {:?}",
            status.end
        )
        .into()
    }
    pub(super) fn startup<T>(
        &self,
        startup: impl Future<Output = Result<T, Error>>,
    ) -> impl Future<Output = Result<T, Error>> {
        // One real startup operation, independent of incoming request roots.
        // Its scoped guard records cancellation or unwinding on disposal.
        record_startup(self.exporter.root(), startup)
    }
    pub(super) async fn finish(self) -> Result<(), Error> {
        let Self {
            mut exporter,
            attachment,
            #[cfg(unix)]
            query,
        } = self;
        // The caller has retired ingress, actors and their owned tasks first.
        drop(attachment);
        let exported = exporter.shutdown(Duration::from_secs(10)).await;
        #[cfg(unix)]
        let query_result = match query {
            Some(query) => query.finish().await,
            None => Ok(()),
        };
        let report = exported?;
        #[cfg(unix)]
        query_result?;
        let snapshot = exporter.snapshot();
        let counts = &snapshot.counts;
        let complete = report.end == TraceExportEnd::Complete
            && report.pending.is_none()
            && snapshot.terminal
            && snapshot.retained == 0
            && counts.overwritten == 0
            && counts.rejected_after_close == 0
            && counts.targets_truncated == 0
            && !counts.counters_overflowed
            && !report.status.counters_overflowed
            && report.status.accepted_spans == counts.completed;
        // Machine-readable bounded receipt, without endpoints, credentials,
        // collector response text or arbitrary actor targets.
        eprintln!(
            "PARROT_APP_TRACE={}",
            serde_json::json!({
                "complete": complete, "end": format!("{:?}", report.end),
                "accepted_spans": report.status.accepted_spans,
                "completed": counts.completed, "in_flight": counts.in_flight,
                "overwritten": counts.overwritten, "rejected_after_close": counts.rejected_after_close,
                "targets_truncated": counts.targets_truncated,
                "retained": snapshot.retained, "terminal": snapshot.terminal,
                "pending_spans": report.pending.as_ref().map_or(0, |p| p.snapshot.records.len()),
                "collector_warnings": report.status.accepted_with_warning,
                "counters_overflowed": counts.counters_overflowed || report.status.counters_overflowed,
            })
        );
        if complete {
            Ok(())
        } else {
            Err("application trace export incomplete; see PARROT_APP_TRACE receipt".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;
    use parrot_api::trace::TraceRecorder;

    #[tokio::test]
    async fn startup_scope_records_outcomes_and_restores_ambient_context() {
        let recorder = TraceRecorder::new(8).unwrap();
        let outer = TraceContext::root();
        outer
            .scope(async {
                let assert_restored = || {
                    assert_eq!(TraceContext::current().unwrap().span_id(), outer.span_id());
                };
                let root = recorder.root();
                let expected = root.span_id();
                let reply = record_startup(root, async {
                    assert_eq!(TraceContext::current().unwrap().span_id(), expected);
                    Ok(19u64)
                })
                .await
                .unwrap();
                assert_eq!(reply, 19);
                assert_restored();
                let error = record_startup(recorder.root(), async {
                    Err::<(), Error>("startup rejected".into())
                })
                .await
                .unwrap_err();
                assert_eq!(error.to_string(), "startup rejected");
                assert_restored();
                let panic =
                    std::panic::AssertUnwindSafe(record_startup::<()>(recorder.root(), async {
                        panic!("startup unwind probe");
                    }))
                    .catch_unwind()
                    .await;
                assert!(panic.is_err());
                assert_restored();
                assert!(
                    record_startup::<()>(recorder.root(), std::future::pending())
                        .now_or_never()
                        .is_none()
                );
                assert_restored();
                // Merely constructing an operation does not claim executed work.
                drop(record_startup::<()>(
                    recorder.root(),
                    std::future::pending(),
                ));
                assert_restored();
            })
            .await;
        assert!(TraceContext::current().is_none());
        recorder.close();
        let snapshot = recorder.drain();
        assert!(snapshot.terminal);
        assert_eq!(snapshot.counts.started, 4);
        assert_eq!(snapshot.counts.in_flight, 0);
        assert_eq!(
            snapshot
                .records
                .iter()
                .map(|record| record.status)
                .collect::<Vec<_>>(),
            vec![
                SpanStatus::Ok,
                SpanStatus::Error,
                SpanStatus::Panicked,
                SpanStatus::Cancelled
            ]
        );
        assert!(snapshot
            .records
            .iter()
            .all(|record| record.kind == SpanKind::Application
                && record.target == "/application/startup"
                && record.parent_span_id.is_none()));
        let traces: std::collections::HashSet<_> = snapshot
            .records
            .iter()
            .map(|record| record.trace_id)
            .collect();
        assert_eq!(traces.len(), 4);
    }
}
