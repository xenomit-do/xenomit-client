use std::sync::atomic::{AtomicUsize, Ordering};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::{Layer, util::SubscriberInitExt};

static STUBS_REPORTED: AtomicUsize = AtomicUsize::new(0);
static WARNINGS_REPORTED: AtomicUsize = AtomicUsize::new(0);
static ERRORS_REPORTED: AtomicUsize = AtomicUsize::new(0);

pub fn install() {
    tracing_subscriber::registry().with(SafeWarnings).init();
}

struct SafeWarnings;

impl<S: Subscriber> Layer<S> for SafeWarnings {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let metadata = event.metadata();
        if !matches!(*metadata.level(), Level::WARN | Level::ERROR) {
            return;
        }
        let (counter, limit) = if *metadata.level() == Level::ERROR {
            (&ERRORS_REPORTED, 80)
        } else if metadata.target() == "ruffle_core::stub" {
            (&STUBS_REPORTED, 8)
        } else {
            (&WARNINGS_REPORTED, 40)
        };
        if counter.fetch_add(1, Ordering::Relaxed) >= limit {
            return;
        }
        let mut message = Message::default();
        event.record(&mut message);
        let summary = redact(&message.0);
        eprintln!(
            "[ruffle] {} {}: {summary}",
            metadata.level(),
            metadata.target()
        );
    }
}

#[derive(Default)]
struct Message(String);

impl Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

/// Trace messages can contain URLs and cookies. Keep the failure class while
/// removing URLs and long opaque values before printing anything.
fn redact(message: &str) -> String {
    let mut safe = String::new();
    for word in message.split_whitespace().take(28) {
        let hidden = word.contains("://")
            || word.len() > 24
            || word.to_ascii_lowercase().contains("sessionid")
            || word.to_ascii_lowercase().contains("dosid")
            || word.to_ascii_lowercase().contains("sid=")
            || word.to_ascii_lowercase().contains("token");
        if !safe.is_empty() {
            safe.push(' ');
        }
        safe.push_str(if hidden { "<redacted>" } else { word });
    }
    safe
}
