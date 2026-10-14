//! Helpers for spans that other crates open.

use std::future::Future;

use tracing::instrument::{Instrument as _, Instrumented};

/// The target of the span [`outside_spans`] runs a future in. Never exported as a trace.
pub const DETACHED_TARGET: &str = "hs_telemetry::detached";

/// Runs `future` in a root span of its own (`outbound`, target [`DETACHED_TARGET`], never
/// exported), so that a task it spawns does not inherit the caller's spans.
///
/// Wrap every outbound HTTP exchange made with `reqwest` (or anything else on `hyper-util`'s
/// client) in this. `hyper-util`, with its `tracing` feature (which `kube-client` turns on for
/// the whole binary), spawns each new connection's task with `in_current_span()`: the task, and
/// so the caller's span and every span above it, lives as long as the pooled connection does.
/// A request that opened a connection to another server was then never closed, so never
/// exported, while that connection stayed in the pool (found 2026-10-14: an inbound `/send`
/// whose key lookup asked a notary never reached the trace store). The connection task holds
/// this span instead, which costs nothing.
///
/// Log lines written inside still reach the log (a resolver's warning, say), under this span
/// rather than the caller's: they say what they are about themselves. Time the exchange with a
/// span held around the call, not entered inside it.
///
/// With a filter that leaves `info` spans of this target out (`RUST_LOG=hs_federation=debug`
/// alone, say) the span is not made, and the caller's spans are held as before; the default
/// filters keep it.
pub fn outside_spans<F: Future>(future: F) -> Instrumented<F> {
    future
        .instrument(tracing::info_span!(target: "hs_telemetry::detached", parent: None, "outbound"))
}

/// Whether the current span's trace is being recorded for export: there is a current span, the
/// OTLP layer is installed, and the trace was sampled.
///
/// The guard for a span around a small, frequent operation (a store transaction, one step of a
/// pass): `if hs_telemetry::current_is_sampled() { info_span!(..) } else { Span::none() }`.
/// At a `sample_ratio` below 1 most requests are not sampled, and their request span still
/// exists (the log uses it); a span opened inside one would cost its creation, its fields and
/// the log layer's bookkeeping for nothing. This check costs a few hundred nanoseconds
/// (`docs/status/12-platform-and-kubernetes.md` has the measurement), and nothing without the
/// `otlp` feature, where it is always `false`.
#[must_use]
pub fn current_is_sampled() -> bool {
    #[cfg(feature = "otlp")]
    {
        crate::otlp::span_context(&tracing::Span::current()).is_sampled()
    }
    #[cfg(not(feature = "otlp"))]
    {
        false
    }
}

/// The W3C `traceparent` of the current span, to send with a call to another replica so its
/// work joins this trace. `None` when there is no current span or no OTLP layer; an unsampled
/// trace is passed on too (flag `00`), so the other side does not sample it on its own.
#[must_use]
pub fn current_traceparent() -> Option<String> {
    #[cfg(feature = "otlp")]
    {
        crate::otlp::traceparent(&crate::otlp::span_context(&tracing::Span::current()))
    }
    #[cfg(not(feature = "otlp"))]
    {
        None
    }
}

/// Makes `span` a child of the remote span `traceparent` names (from [`current_traceparent`] on
/// another replica), so it continues that trace. Call it right after making the span, before it
/// is entered or has children; a later call, an unparseable value or a build without the `otlp`
/// feature leaves the span its own trace's root.
pub fn continue_trace(span: &tracing::Span, traceparent: &str) {
    #[cfg(feature = "otlp")]
    crate::otlp::set_remote_parent(span, crate::otlp::parse_traceparent(traceparent));
    #[cfg(not(feature = "otlp"))]
    let _ = (span, traceparent);
}

/// A reference to a span, kept to link later work to it: the request that queued what a
/// background pass now does. A pass is a trace of its own (it may serve many requests, and it
/// may run long after they answered); a link says which request caused it without making the
/// request's trace as long as the pass.
///
/// Empty (and free) without the `otlp` feature or outside a recorded trace.
#[derive(Debug, Clone, Default)]
pub struct TraceLink {
    #[cfg(feature = "otlp")]
    cx: Option<opentelemetry::trace::SpanContext>,
}

impl TraceLink {
    /// A link to the current span, when its trace is sampled; empty otherwise (a link to a trace
    /// that was never exported leads nowhere).
    #[must_use]
    pub fn current() -> Self {
        #[cfg(feature = "otlp")]
        {
            let cx = crate::otlp::span_context(&tracing::Span::current());
            Self {
                cx: cx.is_sampled().then_some(cx),
            }
        }
        #[cfg(not(feature = "otlp"))]
        {
            Self {}
        }
    }

    /// A link to the span `traceparent` names, when it is a valid, sampled one.
    #[must_use]
    pub fn from_traceparent(traceparent: &str) -> Self {
        #[cfg(feature = "otlp")]
        {
            let cx = crate::otlp::parse_traceparent(traceparent);
            Self {
                cx: (cx.is_valid() && cx.is_sampled()).then_some(cx),
            }
        }
        #[cfg(not(feature = "otlp"))]
        {
            let _ = traceparent;
            Self {}
        }
    }

    /// The linked span as a W3C `traceparent`, to store with queued work.
    #[must_use]
    pub fn traceparent(&self) -> Option<String> {
        #[cfg(feature = "otlp")]
        {
            self.cx.as_ref().and_then(crate::otlp::traceparent)
        }
        #[cfg(not(feature = "otlp"))]
        {
            None
        }
    }

    /// Whether this links to anything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        #[cfg(feature = "otlp")]
        {
            self.cx.is_none()
        }
        #[cfg(not(feature = "otlp"))]
        {
            true
        }
    }

    /// Adds the link to `span` (a pass's root span). Nothing when this is empty.
    pub fn add_to(&self, span: &tracing::Span) {
        #[cfg(feature = "otlp")]
        if let Some(cx) = &self.cx {
            crate::otlp::add_link(span, cx.clone());
        }
        #[cfg(not(feature = "otlp"))]
        let _ = span;
    }
}

/// [`tokio::task::spawn_blocking`], traced: runs `work` on a blocking thread inside the
/// caller's span, and, in a recorded trace, inside a span named `name` (`otel.name`) whose
/// child `blocking_wait` is the time spent waiting for a blocking thread, so a trace shows the
/// wait (a busy blocking pool) apart from the work (decision 0048, rule 5).
///
/// Outside a recorded trace this costs what `spawn_blocking` does plus the sampled check, and
/// the work still runs inside the caller's span, so its log lines keep the request's fields
/// (plain `spawn_blocking` loses them).
///
/// `name` is rendered only in a recorded trace, so `format_args!("store {op}")` costs nothing
/// otherwise. The thread is taken when this is called, as with `spawn_blocking`; the returned
/// future waits for the work.
///
/// # Errors
/// The [`tokio::task::JoinError`] of a task that panicked or was cancelled.
pub fn spawn_blocking<T, F>(
    name: impl std::fmt::Display,
    work: F,
) -> impl Future<Output = Result<T, tokio::task::JoinError>> + Send + 'static
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let caller = tracing::Span::current();
    let (span, wait) = if current_is_sampled() {
        let span = tracing::info_span!("blocking", "otel.name" = name.to_string().as_str());
        let wait = tracing::info_span!(parent: &span, "blocking_wait");
        (span, wait)
    } else {
        (tracing::Span::none(), tracing::Span::none())
    };
    let inside = if span.is_none() { caller } else { span.clone() };
    let task = tokio::task::spawn_blocking(move || {
        drop(wait);
        inside.in_scope(work)
    });
    tracing::Instrument::instrument(task, span)
}

/// Where a closure of type `F` was written, as `Type::function` (the last two segments of its
/// path, generic arguments left out), to name a span after the code that asked for the work:
/// a store transaction's closure written in `AuthTables<B>::get_device` is
/// `AuthTables::get_device`. Read from [`std::any::type_name`], which is meant for diagnostics
/// and promises no format; what this cannot take apart comes out whole. Allocates: call it
/// only for a span that is recorded ([`current_is_sampled`]).
#[must_use]
pub fn caller_of<F: ?Sized>() -> String {
    caller_from_type_name(std::any::type_name::<F>())
}

/// [`caller_of`] for the type of `f`, for a closure passed as `impl FnOnce` (whose type cannot
/// be named).
#[must_use]
pub fn caller_of_value<F: ?Sized>(f: &F) -> String {
    let _ = f;
    caller_of::<F>()
}

/// [`caller_of`]'s work, on a type name.
fn caller_from_type_name(type_name: &str) -> String {
    let mut name = type_name;
    while let Some(outer) = name.strip_suffix("::{{closure}}") {
        name = outer;
    }
    // A qualified path, `<Foo as Trait>::method`: the type is `Foo`.
    let unqualified;
    if let Some(inner) = name.strip_prefix('<') {
        let mut depth = 1usize;
        let close = inner.char_indices().find_map(|(i, c)| {
            match c {
                '<' => depth += 1,
                '>' => depth -= 1,
                _ => {}
            }
            (depth == 0).then_some(i)
        });
        if let Some(close) = close {
            let self_type = inner[..close].split(" as ").next().unwrap_or_default();
            unqualified = format!("{self_type}{}", &inner[close + 1..]);
            name = &unqualified;
        }
    }
    // Leave out every generic argument list (`<B>`, `<hs_kv::memory::MemoryBackend, ..>`).
    let mut plain = String::with_capacity(name.len());
    let mut depth = 0usize;
    for c in name.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => plain.push(c),
            _ => {}
        }
    }
    let segments: Vec<&str> = plain.split("::").filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        [] => name.to_owned(),
        [only] => (*only).to_owned(),
        [.., parent, last] => format!("{parent}::{last}"),
    }
}

/// At most this many links are put on one pass's span ([`add_links`]); a pass that served a
/// thousand requests does not need a thousand links to say what it is.
pub const MAX_LINKS_PER_SPAN: usize = 32;

/// Adds up to [`MAX_LINKS_PER_SPAN`] of `links` to `span`, skipping empty ones, and records how
/// many were left out as `links_dropped` when the span declares that field.
pub fn add_links<'a>(span: &tracing::Span, links: impl IntoIterator<Item = &'a TraceLink>) {
    let mut added = 0usize;
    let mut left_out = 0usize;
    for link in links {
        if link.is_empty() {
            continue;
        }
        if added < MAX_LINKS_PER_SPAN {
            link.add_to(span);
            added += 1;
        } else {
            left_out += 1;
        }
    }
    if left_out > 0 {
        span.record("links_dropped", left_out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt as _;

    #[test]
    fn a_closure_is_named_after_the_function_it_was_written_in() {
        struct Tables<B>(B);
        impl<B> Tables<B> {
            fn get_device(&self) -> String {
                fn name<F>(_: &F) -> String {
                    caller_of::<F>()
                }
                let f = || ();
                name(&f)
            }
        }
        assert_eq!(Tables(1u8).get_device(), "Tables::get_device");
        assert_eq!(
            caller_from_type_name(
                "hs_auth::store::tables::AuthTables<hs_kv::fjall_backend::FjallBackend>::get_device::{{closure}}::{{closure}}"
            ),
            "AuthTables::get_device"
        );
        assert_eq!(
            caller_from_type_name("<hs_x::Foo as hs_y::Store>::save::{{closure}}"),
            "Foo::save"
        );
        assert_eq!(caller_from_type_name("plain"), "plain");
    }

    #[test]
    fn the_detached_target_is_the_one_the_span_is_made_with() {
        // `tracing`'s macros take a literal target; this keeps the constant honest.
        let subscriber = tracing_subscriber::registry();
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(target: "hs_telemetry::detached", "outbound");
            assert_eq!(span.metadata().map(|m| m.target()), Some(DETACHED_TARGET));
        });
    }

    #[tokio::test]
    async fn a_task_spawned_inside_does_not_hold_the_callers_span_open() {
        use std::sync::{Arc, Mutex};

        /// Records the names of closed spans, and the events' messages.
        #[derive(Clone, Default)]
        struct Seen {
            closed: Arc<Mutex<Vec<String>>>,
            events: Arc<Mutex<Vec<String>>>,
        }
        impl<S> tracing_subscriber::Layer<S> for Seen
        where
            S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
        {
            fn on_close(&self, id: tracing::Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
                if let Some(span) = ctx.span(&id) {
                    self.closed.lock().unwrap().push(span.name().to_owned());
                }
            }
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                self.events
                    .lock()
                    .unwrap()
                    .push(event.metadata().name().to_owned());
            }
        }

        let seen = Seen::default();
        let subscriber = tracing_subscriber::registry().with(seen.clone());
        let _default = tracing::subscriber::set_default(subscriber);
        let closed = |name: &str| seen.closed.lock().unwrap().iter().any(|n| n == name);

        let (release, held) = tokio::sync::oneshot::channel::<()>();
        let request = tracing::info_span!("request");
        let spawn_like_hyper = async {
            tracing::warn!("a resolver's warning");
            // `hyper-util`'s executor, with its `tracing` feature.
            tokio::spawn(tracing::Instrument::in_current_span(async move {
                let _ = held.await;
            }));
        };
        tracing::Instrument::instrument(outside_spans(spawn_like_hyper), request).await;
        tokio::task::yield_now().await;
        assert!(closed("request"), "{:?}", seen.closed.lock().unwrap());
        assert!(
            !closed("outbound"),
            "the connection task holds the detached span"
        );
        assert_eq!(
            seen.events.lock().unwrap().len(),
            1,
            "the warning was logged"
        );

        // Without it, the spawned task keeps the request open for as long as it lives.
        let (release_unwrapped, held_unwrapped) = tokio::sync::oneshot::channel::<()>();
        let unwrapped = tracing::info_span!("unwrapped");
        tracing::Instrument::instrument(
            async {
                tokio::spawn(tracing::Instrument::in_current_span(async move {
                    let _ = held_unwrapped.await;
                }));
            },
            unwrapped,
        )
        .await;
        tokio::task::yield_now().await;
        assert!(!closed("unwrapped"));
        let _ = release_unwrapped.send(());
        let _ = release.send(());
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        assert!(closed("unwrapped") && closed("outbound"));
    }
}
