//! Test support, behind the `testing` feature: run code in a trace of its own and get back the
//! spans it exported, so a crate's tests can check the spans its code opens.
//!
//! [`trace`] (and [`trace_sync`]) install, once per test binary, a global subscriber that
//! records every trace (ratio 1) with the OTLP layer the server uses, exporting to memory as
//! each span closes. Each call opens a root span (`test`) of a new trace, runs the code inside
//! it, and returns the spans of that trace only, so tests running at once in the same binary
//! do not see each other's spans. Global, so work moved to another thread (`spawn_blocking`)
//! is recorded too; a test binary that installs another global subscriber cannot use this.
//!
//! ```ignore
//! let (_, spans) = hs_telemetry::testing::trace(async { store.save(..).await }).await;
//! let txn = spans.iter().find(|s| s.name == "kv.transaction").unwrap();
//! assert_eq!(txn.attr("mutations"), Some("2"));
//! ```

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};

use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{SdkTracerProvider, SpanData, SpanExporter};
use tracing::Instrument as _;
use tracing_subscriber::layer::SubscriberExt as _;

/// One exported span, in plain values.
#[derive(Debug, Clone)]
pub struct ExportedSpan {
    /// The span's name as exported (`otel.name` when the code set one).
    pub name: String,
    /// The span's id, hex.
    pub span_id: String,
    /// Its parent's id, hex (`0000000000000000` for a root).
    pub parent_span_id: String,
    /// Its trace's id, hex.
    pub trace_id: String,
    /// Its attributes, rendered as strings.
    pub attributes: BTreeMap<String, String>,
    /// The trace ids of the spans it links to.
    pub links: Vec<String>,
    /// Whether it was marked failed (`otel.status_code = "error"`).
    pub failed: bool,
    /// `server`, `client`, `internal`, ...
    pub kind: String,
}

impl ExportedSpan {
    /// The attribute `key`, rendered as a string.
    #[must_use]
    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attributes.get(key).map(String::as_str)
    }

    fn from_data(span: &SpanData) -> Self {
        Self {
            name: span.name.to_string(),
            span_id: span.span_context.span_id().to_string(),
            parent_span_id: span.parent_span_id.to_string(),
            trace_id: span.span_context.trace_id().to_string(),
            attributes: span
                .attributes
                .iter()
                .map(|kv| (kv.key.to_string(), kv.value.to_string()))
                .collect(),
            links: span
                .links
                .iter()
                .map(|l| l.span_context.trace_id().to_string())
                .collect(),
            failed: matches!(span.status, opentelemetry::trace::Status::Error { .. }),
            kind: format!("{:?}", span.span_kind).to_lowercase(),
        }
    }
}

/// The spans of one test's trace, with helpers for finding things in it.
#[derive(Debug, Clone, Default)]
pub struct Trace(pub Vec<ExportedSpan>);

impl Trace {
    /// The first span named `name`.
    #[must_use]
    pub fn find(&self, name: &str) -> Option<&ExportedSpan> {
        self.0.iter().find(|s| s.name == name)
    }

    /// Every span named `name`.
    #[must_use]
    pub fn all(&self, name: &str) -> Vec<&ExportedSpan> {
        self.0.iter().filter(|s| s.name == name).collect()
    }

    /// The names of every span, in the order they closed.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.0.iter().map(|s| s.name.as_str()).collect()
    }

    /// Whether `child` is a descendant of `ancestor` in this trace.
    #[must_use]
    pub fn is_under(&self, child: &ExportedSpan, ancestor: &ExportedSpan) -> bool {
        let mut parent = child.parent_span_id.clone();
        for _ in 0..self.0.len() {
            if parent == ancestor.span_id {
                return true;
            }
            match self.0.iter().find(|s| s.span_id == parent) {
                Some(next) => parent = next.parent_span_id.clone(),
                None => return false,
            }
        }
        false
    }
}

#[derive(Debug, Clone, Default)]
struct Memory(Arc<Mutex<Vec<SpanData>>>);

impl SpanExporter for Memory {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(batch);
        Ok(())
    }
}

struct Installed {
    memory: Memory,
    provider: SdkTracerProvider,
}

static INSTALLED: OnceLock<Installed> = OnceLock::new();

/// Installs the recording subscriber, if it is not yet: call it before code whose root spans a
/// test reads with [`traces_rooted_at`]. [`trace`] and [`trace_sync`] install it themselves.
pub fn install() {
    installed();
}

fn installed() -> &'static Installed {
    INSTALLED.get_or_init(|| {
        let memory = Memory::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(memory.clone())
            .with_sampler(crate::otlp::sampler(1.0))
            .build();
        let subscriber =
            tracing_subscriber::registry().with(crate::otlp::layer_from_provider(&provider));
        if tracing::subscriber::set_global_default(subscriber).is_err() {
            panic!(
                "hs_telemetry::testing needs to install the global subscriber, and this test \
                 binary has one already"
            );
        }
        Installed { memory, provider }
    })
}

/// The spans exported so far whose trace is `trace_id`, taken out of the store.
fn take(installed: &Installed, trace_id: &str) -> Trace {
    let _ = installed.provider.force_flush();
    let mut all = installed
        .memory
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut mine = Vec::new();
    all.retain(|span| {
        if span.span_context.trace_id().to_string() == trace_id {
            mine.push(ExportedSpan::from_data(span));
            false
        } else {
            true
        }
    });
    Trace(mine)
}

fn root() -> (tracing::Span, String) {
    let root = tracing::info_span!(target: "hs_telemetry::testing", parent: None, "test");
    let trace_id = crate::otlp::span_context(&root).trace_id().to_string();
    (root, trace_id)
}

/// Runs `future` in a new trace (root span `test`) and returns its output and the trace's
/// spans. See the module docs.
pub async fn trace<F: Future>(future: F) -> (F::Output, Trace) {
    let installed = installed();
    let (root, trace_id) = root();
    let out = future.instrument(root).await;
    (out, take(installed, &trace_id))
}

/// [`trace`] for synchronous code.
pub fn trace_sync<T>(f: impl FnOnce() -> T) -> (T, Trace) {
    let installed = installed();
    let (root, trace_id) = root();
    let out = root.in_scope(f);
    drop(root);
    (out, take(installed, &trace_id))
}

/// The spans of trace `trace_id` exported since they were last taken: what a test's trace
/// held that had not closed yet when [`trace`] returned.
#[must_use]
pub fn spans_of_trace(trace_id: &str) -> Trace {
    take(installed(), trace_id)
}

/// The spans of a trace that is not the caller's (a background pass's, which is a root of its
/// own): every span exported so far whose trace has a span named `root_name`, the latest such
/// trace first. Spans that are not yet closed are not there.
#[must_use]
pub fn traces_rooted_at(root_name: &str) -> Vec<Trace> {
    let installed = installed();
    let _ = installed.provider.force_flush();
    let ids: Vec<String> = {
        let all = installed
            .memory
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut ids: Vec<String> = all
            .iter()
            .rev()
            .filter(|s| s.name == root_name)
            .map(|s| s.span_context.trace_id().to_string())
            .collect();
        ids.dedup();
        ids
    };
    ids.iter().map(|id| take(installed, id)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn blocking_work_is_a_span_with_its_wait_for_a_thread_apart() {
        let (value, spans) = trace(async {
            crate::spans::spawn_blocking("store.read", || {
                let _inside = tracing::info_span!(target: "hs_test", "work.inside").entered();
                7
            })
            .await
            .unwrap()
        })
        .await;
        assert_eq!(value, 7);
        let blocking = spans.find("store.read").expect("the blocking span");
        let wait = spans.find("blocking_wait").expect("the wait");
        let inside = spans.find("work.inside").expect("the work's own span");
        assert_eq!(wait.parent_span_id, blocking.span_id);
        assert_eq!(inside.parent_span_id, blocking.span_id);
        assert!(spans.is_under(blocking, spans.find("test").unwrap()));
    }

    #[tokio::test]
    async fn a_test_gets_the_spans_of_its_own_trace_across_threads() {
        let ((), spans) = trace(async {
            let _outer = tracing::info_span!(target: "hs_test", "outer").entered();
            let inner = tracing::info_span!(target: "hs_test", "on.another.thread");
            std::thread::spawn(move || drop(inner.entered()))
                .join()
                .unwrap();
        })
        .await;
        let ((), other) =
            trace_sync(|| drop(tracing::info_span!(target: "hs_test", "elsewhere").entered()));
        assert!(spans.find("outer").is_some(), "{:?}", spans.names());
        let moved = spans.find("on.another.thread").expect("the moved span");
        let test = spans.find("test").expect("the root");
        assert!(spans.is_under(moved, test));
        assert!(spans.find("elsewhere").is_none());
        assert!(other.find("elsewhere").is_some());
    }
}
