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

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt as _;

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
