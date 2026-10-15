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
//! A span that closes after the code returned (a background task's, another replica's) is
//! waited for with a deadline, never slept for: [`trace_until`] for the caller's own trace,
//! [`wait_for_trace_rooted_at`] for a background pass's.
//!
//! Every entry point that starts recording ([`install`], [`trace`], [`trace_sync`],
//! [`trace_until`]) rebuilds `tracing`'s callsite interest cache once the recorder is the
//! global default. `tracing-core` caches, per callsite, whether any subscriber wants its spans,
//! and a callsite first reached on another thread while the global default is being set can
//! cache "never" (asked of the no-op dispatcher that is still the default) after the
//! recorder's own registration rebuilt it: that span is then not made again in the process.
//! In a test binary the other tests keep running while one installs the recorder, so this can
//! happen. The rebuild, made after the global default is set and before the test's code runs,
//! corrects it.
//!
//! The recorder makes [`crate::outside_spans`]'s span (it does not export it), as the server's
//! log layers do: with no layer wanting it, the span was not made, the caller's span stayed
//! current inside the wrapper, and a connection task `hyper-util` spawned there held the
//! caller's trace open for as long as the pooled connection lived. That is why a sender test
//! never saw its own `federation.transaction` close in the workspace gate (2026-10-14). The
//! `testing` feature turns on `hyper-util`'s `tracing` feature, which spawns connection tasks
//! that way, so `cargo test -p <crate>` sees what the workspace build (and the shipped binary,
//! through `kube-client`) does; without it a test could pass alone and fail in the gate.
//!
//! ```ignore
//! let (_, spans) = hs_telemetry::testing::trace(async { store.save(..).await }).await;
//! let txn = spans.iter().find(|s| s.name == "kv.transaction").unwrap();
//! assert_eq!(txn.attr("mutations"), Some("2"));
//! ```

use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{SdkTracerProvider, SpanData, SpanExporter};
use tracing::Instrument as _;
use tracing_subscriber::Layer as _;
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

/// Where the recorder's spans go: every exported span until a test takes it, and a signal to
/// the waiters each time more arrive.
#[derive(Debug, Clone, Default)]
struct Memory {
    spans: Arc<Mutex<Vec<SpanData>>>,
    exported: Arc<tokio::sync::Notify>,
}

impl Memory {
    fn lock(&self) -> MutexGuard<'_, Vec<SpanData>> {
        self.spans
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl SpanExporter for Memory {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        self.lock().extend(batch);
        self.exported.notify_waiters();
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
    recording();
}

/// [`installed`], with every callsite's cached interest rebuilt now that the recorder is the
/// global default (see the module docs). Called where recording starts, before the code under
/// test runs, so a callsite whose first use raced with the install is enabled again by then.
fn recording() -> &'static Installed {
    let installed = installed();
    tracing::callsite::rebuild_interest_cache();
    installed
}

fn installed() -> &'static Installed {
    INSTALLED.get_or_init(|| {
        let memory = Memory::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(memory.clone())
            .with_sampler(crate::otlp::sampler(1.0))
            .build();
        // The second layer wants only `outside_spans`'s span, which the OTLP layer leaves out:
        // with no layer wanting it the registry would not make it, so it would not become the
        // current span, and a task spawned inside it would hold the caller's spans open (see
        // the module docs). In the server the log layers want it.
        let subscriber = tracing_subscriber::registry()
            .with(crate::otlp::layer_from_provider(&provider))
            .with(tracing_subscriber::layer::Identity::new().with_filter(
                tracing_subscriber::filter::filter_fn(|metadata| {
                    metadata.target() == crate::spans::DETACHED_TARGET
                }),
            ));
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
    take_from(&mut installed.memory.lock(), trace_id)
}

/// The spans in `spans` whose trace is `trace_id`, taken out.
fn take_from(spans: &mut Vec<SpanData>, trace_id: &str) -> Trace {
    let mut mine = Vec::new();
    spans.retain(|span| {
        if span.span_context.trace_id().to_string() == trace_id {
            mine.push(ExportedSpan::from_data(span));
            false
        } else {
            true
        }
    });
    Trace(mine)
}

/// The spans in `spans` whose trace is `trace_id`, left where they are.
fn copy_from(spans: &[SpanData], trace_id: &str) -> Trace {
    Trace(
        spans
            .iter()
            .filter(|span| span.span_context.trace_id().to_string() == trace_id)
            .map(ExportedSpan::from_data)
            .collect(),
    )
}

/// The ids of the traces in `spans` that have a span named `name`, the latest first, each once.
fn ids_with(spans: &[SpanData], name: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    spans
        .iter()
        .rev()
        .filter(|span| span.name == name)
        .map(|span| span.span_context.trace_id().to_string())
        .filter(|id| seen.insert(id.clone()))
        .collect()
}

fn root() -> (tracing::Span, String) {
    let root = tracing::info_span!(target: "hs_telemetry::testing", parent: None, "test");
    let trace_id = crate::otlp::span_context(&root).trace_id().to_string();
    (root, trace_id)
}

/// Runs `future` in a new trace (root span `test`) and returns its output and the trace's
/// spans. See the module docs.
pub async fn trace<F: Future>(future: F) -> (F::Output, Trace) {
    let installed = recording();
    let (root, trace_id) = root();
    let out = future.instrument(root).await;
    (out, take(installed, &trace_id))
}

/// [`trace`], then waits for the spans of the trace that close after `future` returned (a
/// task's that outlives it, or another replica's that continued the trace) until `done` holds
/// of the spans so far, or `deadline` passes. The spans so far are returned either way, so the
/// test's own assertions say what was missing.
pub async fn trace_until<F: Future>(
    future: F,
    deadline: Duration,
    done: impl Fn(&Trace) -> bool,
) -> (F::Output, Trace) {
    let installed = recording();
    let (root, trace_id) = root();
    let out = future.instrument(root).await;
    let mut spans = take(installed, &trace_id);
    if !done(&spans) {
        let _ = wait_until(deadline, || {
            spans.0.extend(take(installed, &trace_id).0);
            done(&spans).then_some(())
        })
        .await;
    }
    (out, spans)
}

/// [`trace`] for synchronous code.
pub fn trace_sync<T>(f: impl FnOnce() -> T) -> (T, Trace) {
    let installed = recording();
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
    let mut all = installed.memory.lock();
    ids_with(&all, root_name)
        .iter()
        .map(|id| take_from(&mut all, id))
        .collect()
}

/// Waits for the trace of a background pass (one with a span named `root_name`, a root of its
/// own) that `matches` holds of, until `deadline` passes, and takes it out of the store.
/// Traces that do not match are left, so tests running at once that each wait for their own
/// pass of one kind do not take each other's.
///
/// # Errors
/// No matching trace was exported before `deadline`: the error holds every trace with a span
/// named `root_name` that was (left in the store), for the test's failure message.
pub async fn wait_for_trace_rooted_at(
    root_name: &str,
    deadline: Duration,
    matches: impl Fn(&Trace) -> bool,
) -> Result<Trace, Vec<Trace>> {
    let installed = installed();
    let found = wait_until(deadline, || {
        let mut all = installed.memory.lock();
        let id = ids_with(&all, root_name)
            .into_iter()
            .find(|id| matches(&copy_from(&all, id)))?;
        Some(take_from(&mut all, &id))
    })
    .await;
    found.ok_or_else(|| {
        let all = installed.memory.lock();
        ids_with(&all, root_name)
            .iter()
            .map(|id| copy_from(&all, id))
            .collect()
    })
}

/// Calls `check` until it returns something: now, then each time more spans are exported, and
/// once more when `deadline` has passed. Woken by the exports, not by polling.
async fn wait_until<T>(deadline: Duration, mut check: impl FnMut() -> Option<T>) -> Option<T> {
    let installed = installed();
    let until = tokio::time::Instant::now() + deadline;
    loop {
        // Registered before the check, so an export between the check and the wait wakes it.
        let mut exported = std::pin::pin!(installed.memory.exported.notified());
        exported.as_mut().enable();
        let _ = installed.provider.force_flush();
        if let Some(found) = check() {
            return Some(found);
        }
        if tokio::time::timeout_at(until, exported).await.is_err() {
            return check();
        }
    }
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

    #[tokio::test]
    async fn a_span_that_closes_after_the_code_returned_is_waited_for() {
        let looks = std::sync::atomic::AtomicUsize::new(0);
        let ((), spans) = trace_until(
            async {
                // Held by a task that runs only once the test's task waits: on this runtime's
                // one thread, not before `trace_until` has looked once and missed it.
                let late = tracing::info_span!(target: "hs_test", "closes.later");
                tokio::spawn(async move { drop(late) });
            },
            Duration::from_secs(10),
            |spans| {
                looks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                spans.find("closes.later").is_some() && spans.find("test").is_some()
            },
        )
        .await;
        assert!(
            looks.into_inner() > 1,
            "found by waiting, not on the first look"
        );
        let late = spans.find("closes.later").expect("the late span");
        assert!(spans.is_under(late, spans.find("test").expect("the root")));
    }

    /// `outside_spans` works under the recorder as it does in the server: its `outbound` span is
    /// made (the recorder does not export it), so a task spawned inside it the way `hyper-util`
    /// spawns a connection's does not hold the test's spans open. Before 2026-10-14 no layer
    /// of the recorder wanted that span, so it was not made, the caller's span stayed current,
    /// and a sender test's transaction was held open by its pooled connection until the test
    /// gave up waiting for it (in the workspace gate, where `hyper-util`'s `tracing` is on).
    #[tokio::test]
    async fn a_task_spawned_outside_spans_does_not_hold_the_trace_open() {
        let (release, held) = tokio::sync::oneshot::channel::<()>();
        let ((), spans) = trace_until(
            async {
                let request = tracing::info_span!(target: "hs_test", "request.with.a.connection");
                tracing::Instrument::instrument(
                    crate::outside_spans(async {
                        assert_eq!(
                            tracing::Span::current().metadata().map(|m| m.target()),
                            Some(crate::spans::DETACHED_TARGET),
                            "the detached span is made under the recorder, and is current"
                        );
                        // `hyper-util`'s executor, with its `tracing` feature.
                        tokio::spawn(tracing::Instrument::in_current_span(async move {
                            let _ = held.await;
                        }));
                    }),
                    request,
                )
                .await;
            },
            Duration::from_secs(10),
            |spans| spans.find("request.with.a.connection").is_some(),
        )
        .await;
        assert!(
            spans.find("request.with.a.connection").is_some(),
            "exported while the spawned task still lives: {:?}",
            spans.names()
        );
        assert!(
            spans.find("outbound").is_none(),
            "the detached span is not exported"
        );
        let _ = release.send(());
    }

    #[tokio::test]
    async fn a_background_pass_is_waited_for_and_other_passes_are_left() {
        install();
        let pass = |which: &'static str| {
            tokio::spawn(async move {
                tokio::task::yield_now().await;
                drop(
                    tracing::info_span!(target: "hs_test", parent: None, "test.pass", which)
                        .entered(),
                );
            })
        };
        pass("theirs");
        pass("mine");
        let mine = wait_for_trace_rooted_at("test.pass", Duration::from_secs(10), |trace| {
            trace.find("test.pass").and_then(|s| s.attr("which")) == Some("mine")
        })
        .await
        .expect("my pass");
        assert_eq!(mine.0.len(), 1);
        let theirs = wait_for_trace_rooted_at("test.pass", Duration::from_secs(10), |trace| {
            trace.find("test.pass").and_then(|s| s.attr("which")) == Some("theirs")
        })
        .await;
        assert!(theirs.is_ok(), "the other pass was left in the store");

        let none = wait_for_trace_rooted_at("test.pass", Duration::from_millis(50), |_| true).await;
        assert!(none.unwrap_err().is_empty());
    }
}
