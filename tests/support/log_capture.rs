//! Shared log-capture harness for integration tests.
//!
//! The capture relies on a thread-local event buffer, so the closure passed to
//! [`with_event_capture`] **must** be a current-thread, non-spawning async
//! block. Events produced on other threads (e.g. tasks spawned with
//! `tokio::spawn`) will not appear in the captured list because the buffer is
//! stored in a `thread_local!`.

#![allow(dead_code)]

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use tracing_subscriber::layer::SubscriberExt as _;

/// A single captured tracing event with its message and structured fields.
#[derive(Debug, Default, Clone)]
pub struct CapturedEvent {
    pub message: String,
    pub fields: Vec<(String, String)>,
}

// Global layer with no per-instance state; events are routed to a thread-local buffer.
struct EventCaptureLayer;

#[derive(Default)]
struct EventVisitor {
    event: CapturedEvent,
}

impl tracing::field::Visit for EventVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.event.message = value.to_string();
        } else {
            self.event
                .fields
                .push((field.name().to_string(), value.to_string()));
        }
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.event
            .fields
            .push((field.name().to_string(), value.to_string()));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.event
            .fields
            .push((field.name().to_string(), value.to_string()));
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.event.message = format!("{value:?}");
        } else {
            self.event
                .fields
                .push((field.name().to_string(), format!("{value:?}")));
        }
    }
}

// Per-test event buffer; only populated inside `with_event_capture`.
thread_local! {
    static CAPTURE_EVENTS: RefCell<Option<Arc<Mutex<Vec<CapturedEvent>>>>> = const { RefCell::new(None) };
}

static GLOBAL_CAPTURE_INIT: std::sync::OnceLock<()> = std::sync::OnceLock::new();

// Install the process-global subscriber once. `set_global_default` internally
// calls `rebuild_interest_cache()`, promoting any callsites already cached as
// `Interest::never()` (registered before a subscriber existed) to
// `Interest::always()`.
fn ensure_global_capture() {
    GLOBAL_CAPTURE_INIT.get_or_init(|| {
        let subscriber = tracing_subscriber::registry().with(EventCaptureLayer);
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventCaptureLayer {
    fn register_callsite(
        &self,
        _: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::always()
    }

    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        CAPTURE_EVENTS.with(|te| {
            if let Some(events) = te.borrow().as_ref() {
                let mut visitor = EventVisitor::default();
                event.record(&mut visitor);
                events.lock().unwrap().push(visitor.event);
            }
        });
    }
}

/// Run `f` with tracing events captured into a per-test buffer.
///
/// Returns all events emitted during `f`. **Constraint**: `f` must be a
/// current-thread, non-spawning closure — see the module-level doc for details.
pub async fn with_event_capture<F, Fut>(f: F) -> Vec<CapturedEvent>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    ensure_global_capture();
    let events = Arc::new(Mutex::new(Vec::new()));
    CAPTURE_EVENTS.with(|te| *te.borrow_mut() = Some(events.clone()));
    f().await;
    CAPTURE_EVENTS.with(|te| *te.borrow_mut() = None);
    Arc::try_unwrap(events).unwrap().into_inner().unwrap()
}

/// Look up a named field on the first captured event whose message equals `message`.
pub fn field(events: &[CapturedEvent], message: &str, name: &str) -> Option<String> {
    events
        .iter()
        .find(|event| event.message == message)
        .and_then(|event| {
            event
                .fields
                .iter()
                .find(|(field, _)| field == name)
                .map(|(_, value)| value.clone())
        })
}
