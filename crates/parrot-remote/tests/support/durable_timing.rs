//! Test-only bounded diagnostics. No IO on the observed execution path.
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex, OnceLock},
};
use tracing::{
    field::{Field, Visit},
    span::{Attributes, Id, Record},
    Event, Subscriber,
};
use tracing_subscriber::{
    layer::{Context, SubscriberExt},
    registry::LookupSpan,
    Layer,
};
const TARGET: &str = "parrot::durable::timing";
pub type Fields = BTreeMap<String, Value>;
struct Visitor<'a>(&'a mut Fields);
impl Visit for Visitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0
            .insert(field.name().into(), format!("{value:?}").into());
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().into(), value.into());
    }
}
#[derive(Default)]
pub struct Capture {
    events: Mutex<Recording>,
}
#[derive(Clone, Default, serde::Serialize)]
struct Recording {
    events: Vec<Fields>,
    dropped_events: u64,
}
impl Capture {
    #[allow(dead_code)]
    pub fn events(&self) -> Vec<Fields> {
        let recording = self.events.lock().unwrap();
        assert_eq!(recording.dropped_events, 0, "diagnostic capture overflow");
        recording.events.clone()
    }
}
#[derive(Clone)]
struct Observer(Arc<Capture>);
impl<S> Layer<S> for Observer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut fields = Fields::new();
        attrs.record(&mut Visitor(&mut fields));
        ctx.span(id).unwrap().extensions_mut().insert(fields);
    }
    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let span = ctx.span(id).unwrap();
        let mut extensions = span.extensions_mut();
        if let Some(fields) = extensions.get_mut::<Fields>() {
            values.record(&mut Visitor(fields));
        }
    }
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let mut fields = Fields::new();
        if let Some(span) = ctx.event_span(event) {
            for ancestor in span.scope().from_root() {
                if let Some(parent) = ancestor.extensions().get::<Fields>() {
                    fields.extend(parent.clone());
                }
            }
            fields.insert("diagnostic_span_id".into(), span.id().into_u64().into());
        }
        event.record(&mut Visitor(&mut fields));
        static START: OnceLock<std::time::Instant> = OnceLock::new();
        let elapsed = START.get_or_init(std::time::Instant::now).elapsed();
        fields.insert(
            "observed_ns".into(),
            (elapsed.as_nanos().min(u64::MAX as u128) as u64).into(),
        );
        fields.insert(
            "thread".into(),
            format!("{:?}", std::thread::current().id()).into(),
        );
        let mut recording = self.0.events.lock().unwrap();
        if recording.events.len() < 262_144 {
            let ordinal = recording.events.len() as u64;
            fields.insert("ordinal".into(), ordinal.into());
            recording.events.push(fields);
        } else {
            recording.dropped_events = recording.dropped_events.saturating_add(1);
        }
    }
}
pub fn observer() -> Arc<Capture> {
    static CAPTURE: OnceLock<Arc<Capture>> = OnceLock::new();
    CAPTURE
        .get_or_init(|| {
            let capture = Arc::new(Capture::default());
            tracing::subscriber::set_global_default(
                tracing_subscriber::registry()
                    .with(tracing_subscriber::filter::filter_fn(|metadata| {
                        metadata.target() == TARGET
                    }))
                    .with(Observer(capture.clone())),
            )
            .unwrap();
            capture
        })
        .clone()
}
// cloud_proxy enables this only on explicit request. Snapshotting happens on
// Scratch retirement (including assertion unwinding), outside ACK deadlines.
#[allow(dead_code)]
pub fn optional_init() {
    if std::env::var_os("PARROT_DURABLE_DIAGNOSTICS").is_some() {
        observer();
    }
}
#[allow(dead_code)]
pub fn optional_flush() {
    let Some(root) = std::env::var_os("PARROT_DURABLE_DIAGNOSTICS") else {
        return;
    };
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        use std::io::Write;
        static FILE: OnceLock<Mutex<Option<std::fs::File>>> = OnceLock::new();
        // Serialize snapshots, then release the event lock before file IO.
        // Other tests may still be recording; enabling diagnostics has overhead.
        let mut output = FILE.get_or_init(|| Mutex::new(None)).lock().unwrap();
        let capture = observer();
        let recording = capture.events.lock().unwrap().clone();
        if output.is_none() {
            std::fs::create_dir_all(&root)?;
            *output = Some(
                std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(
                        std::path::PathBuf::from(root)
                            .join(format!("cloud-proxy-{}.json", std::process::id())),
                    )?,
            );
        }
        let file = output.as_mut().unwrap();
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(0))?;
        file.set_len(0)?;
        let mut writer = std::io::BufWriter::new(&mut *file);
        serde_json::to_writer(&mut writer, &recording)?;
        writer.flush()?;
        Ok(())
    })();
    if let Err(error) = result {
        if std::thread::panicking() {
            eprintln!("diagnostic snapshot failed: {error}");
        } else {
            panic!("diagnostic snapshot failed: {error}");
        }
    }
}
