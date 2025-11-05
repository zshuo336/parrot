use ::tracing::{
    Event, Level, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id},
};
use parrot as bird;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tracing_subscriber::{Layer, layer::Context, prelude::*};

// Public macros must resolve their own dependency, even when the caller has a
// same-named module. The external consumer probe also omits tracing entirely.
mod tracing {}

#[derive(Debug, PartialEq)]
struct Record {
    name: String,
    level: Level,
    fields: BTreeMap<String, String>,
}
#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<Record>>>);
struct Fields(BTreeMap<String, String>);
impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
}
impl<S: Subscriber> Layer<S> for Capture {
    fn on_new_span(&self, attrs: &Attributes<'_>, _: &Id, _: Context<'_, S>) {
        let mut fields = Fields(BTreeMap::new());
        attrs.record(&mut fields);
        self.0.lock().unwrap().push(Record {
            name: attrs.metadata().name().to_owned(),
            level: *attrs.metadata().level(),
            fields: fields.0,
        });
    }
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let mut fields = Fields(BTreeMap::new());
        event.record(&mut fields);
        self.0.lock().unwrap().push(Record {
            name: event.metadata().name().to_owned(),
            level: *event.metadata().level(),
            fields: fields.0,
        });
    }
}
fn check(run: impl FnOnce(), span: Option<&str>, level: Level, expected: &[(&str, &str)]) {
    let capture = Capture(Arc::new(Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    ::tracing::subscriber::with_default(subscriber, run);
    let records = capture.0.lock().unwrap();
    assert_eq!(records.len(), 2, "both public macro forms must emit once");
    for (index, record) in records.iter().enumerate() {
        if let Some(name) = span {
            assert_eq!(record.name, name);
        }
        assert_eq!(record.level, level);
        let mut fields: BTreeMap<_, _> = expected
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        if index == 1 {
            fields.insert("count".into(), "7".into());
        }
        assert_eq!(record.fields, fields);
    }
}
#[test]
fn actor_span_preserves_identity_and_extra_fields() {
    check(
        || {
            let _span = bird::actor_span!("worker", "one");
            let _span = bird::actor_span!("worker", "one", count = 7);
        },
        Some("actor"),
        Level::INFO,
        &[("type", "\"worker\""), ("id", "\"one\"")],
    );
}
#[test]
fn message_span_preserves_protocol_and_extra_fields() {
    check(
        || {
            let _span = bird::message_span!("request");
            let _span = bird::message_span!("request", count = 7);
        },
        Some("message"),
        Level::DEBUG,
        &[("type", "\"request\"")],
    );
}
#[test]
fn system_span_preserves_operation_and_extra_fields() {
    check(
        || {
            let _span = bird::system_span!("startup");
            let _span = bird::system_span!("startup", count = 7);
        },
        Some("system"),
        Level::INFO,
        &[("operation", "\"startup\"")],
    );
}
#[test]
fn lifecycle_preserves_identity_and_event() {
    check(
        || {
            bird::log_lifecycle!("worker", "one", "started");
            bird::log_lifecycle!("worker", "one", "started", count = 7);
        },
        None,
        Level::INFO,
        &[
            ("actor_type", "\"worker\""),
            ("actor_id", "\"one\""),
            ("event", "\"started\""),
        ],
    );
}
#[test]
fn message_event_preserves_protocol_and_status() {
    check(
        || {
            bird::log_message!("request", "received");
            bird::log_message!("request", "received", count = 7);
        },
        None,
        Level::DEBUG,
        &[("message_type", "\"request\""), ("status", "\"received\"")],
    );
}
#[test]
fn system_event_preserves_operation_and_status() {
    check(
        || {
            bird::log_system!("startup", "complete");
            bird::log_system!("startup", "complete", count = 7);
        },
        None,
        Level::INFO,
        &[("operation", "\"startup\""), ("status", "\"complete\"")],
    );
}
#[test]
fn error_event_uses_display_and_retains_context() {
    check(
        || {
            let error = std::io::Error::other("consumer error");
            bird::log_error!(error);
            bird::log_error!(error, count = 7);
        },
        None,
        Level::ERROR,
        &[("error", "consumer error")],
    );
}
#[test]
fn scheduler_event_preserves_pool_and_event() {
    check(
        || {
            bird::log_scheduler!("pool", "scheduled");
            bird::log_scheduler!("pool", "scheduled", count = 7);
        },
        None,
        Level::DEBUG,
        &[("scheduler", "\"pool\""), ("event", "\"scheduled\"")],
    );
}
