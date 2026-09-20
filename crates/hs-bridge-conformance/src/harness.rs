//! Wires together this crate's real components the way a real server assembling `hs-appservice`
//! and `hs-auth` would, over an in-memory `hs-kv` backend.

use std::sync::Arc;

use hs_appservice::auth_registry::RegistryAppserviceAdapter;
use hs_appservice::ping::{HttpPingTransport, PingService};
use hs_appservice::query::{HttpQueryTransport, QueryService};
use hs_appservice::registration::Registration;
use hs_appservice::registry::Registry;
use hs_appservice::scheduler::{HttpTransactionSender, Scheduler};
use hs_auth::clock::{Clock, FixedClock};
use hs_kv::memory::MemoryBackend;
use ruma::server_name;

/// Everything one conformance scenario needs, over a shared in-memory backend and a shared,
/// controllable clock (so backoff/retry scenarios do not need real `sleep`s).
pub struct Harness {
    /// The registry every scenario registers bridges into.
    pub registry: Arc<Registry<MemoryBackend>>,
    /// The real HTTP-backed scheduler (transactions actually cross the loopback interface to
    /// whatever `url` a scenario's [`crate::FakeBridge`] is bound to).
    pub scheduler: Scheduler<MemoryBackend>,
    /// The real HTTP-backed ping service.
    pub ping: PingService<MemoryBackend>,
    /// The real HTTP-backed query service.
    pub query: QueryService<MemoryBackend>,
    /// `hs-auth`'s shared state, with its `appservices` field wired to this harness's registry
    /// through [`RegistryAppserviceAdapter`] — the actual production adapter, not a test double,
    /// so identity assertion and device masquerading scenarios exercise `hs-auth`'s real
    /// middleware end to end.
    pub auth_state: hs_auth::state::AuthState,
    /// The clock every component above shares; advance it to test backoff without sleeping.
    pub clock: Arc<FixedClock>,
}

impl Harness {
    /// A fresh harness for `example.org`, with the clock starting at a fixed, arbitrary instant.
    #[must_use]
    pub fn new() -> Self {
        let backend = MemoryBackend::new();
        let clock = Arc::new(FixedClock::new(1_700_000_000_000));
        let registry = Arc::new(
            Registry::open(backend, server_name!("example.org"))
                .expect("opening a registry over a fresh in-memory backend never fails")
                .with_clock(clock.clone() as Arc<dyn Clock>),
        );

        let scheduler = Scheduler::new(
            registry.clone(),
            clock.clone() as Arc<dyn Clock>,
            Arc::new(HttpTransactionSender::new()),
        );
        let ping = PingService::new(registry.clone(), Arc::new(HttpPingTransport::new()));
        let query = QueryService::new(registry.clone(), Arc::new(HttpQueryTransport::new()));

        let adapter = Arc::new(RegistryAppserviceAdapter::new(registry.clone()));
        let auth_state = hs_auth::state::AuthState::in_memory().with_appservices(adapter);

        Self {
            registry,
            scheduler,
            ping,
            query,
            auth_state,
            clock,
        }
    }

    /// Registers a bridge shaped like a real mautrix registration (Appendix B's field list),
    /// pointed at `bridge_url` (typically a spawned [`crate::FakeBridge`]), with a fully
    /// exclusive `users`/`aliases` namespace and every MSC flag on — the maximal case most
    /// scenarios want; scenarios that need a narrower registration build their own
    /// `Registration` and call `harness.registry.add` directly.
    ///
    /// # Panics
    /// Panics if `id`/tokens/namespace collide with something already registered in this
    /// harness — a scenario bug, not an expected outcome to handle gracefully.
    pub fn register_full_featured_bridge(&self, id: &str, bridge_url: Option<&str>) {
        let reg = Registration {
            id: id.to_string(),
            url: bridge_url.map(str::to_string),
            as_token: format!("as_token_{id}"),
            hs_token: format!("hs_token_{id}"),
            sender_localpart: format!("{id}bot"),
            rate_limited: false,
            namespaces: hs_appservice::namespace::Namespaces {
                users: vec![
                    hs_appservice::namespace::NamespaceRule::compile(
                        &format!("^@{id}_.*:example\\.org$"),
                        true,
                    )
                    .unwrap(),
                ],
                aliases: vec![
                    hs_appservice::namespace::NamespaceRule::compile(
                        &format!("^#{id}_.*:example\\.org$"),
                        true,
                    )
                    .unwrap(),
                ],
                rooms: vec![],
            },
            protocols: vec![id.to_string()],
            receive_ephemeral: true,
            push_ephemeral_legacy: true,
            msc3202: true,
            msc4190: true,
            extra: Default::default(),
        };
        self.registry
            .add(&reg)
            .expect("scenario registration must not collide");
    }

    /// Registers a double-puppeting registration for `id`: `url: null`, a broad non-exclusive
    /// `users` namespace covering every local user — exactly the shape `PLAN.md` section 8.2
    /// describes as "how double puppeting works".
    ///
    /// # Panics
    /// Panics on a registration collision (scenario bug).
    pub fn register_double_puppet(&self, id: &str) {
        let reg = Registration {
            id: id.to_string(),
            url: None,
            as_token: format!("as_token_{id}"),
            hs_token: format!("hs_token_{id}"),
            sender_localpart: format!("{id}bot"),
            rate_limited: true,
            namespaces: hs_appservice::namespace::Namespaces {
                users: vec![
                    hs_appservice::namespace::NamespaceRule::compile("@.*:example\\.org", false)
                        .unwrap(),
                ],
                aliases: vec![],
                rooms: vec![],
            },
            protocols: vec![],
            receive_ephemeral: false,
            push_ephemeral_legacy: false,
            msc3202: false,
            msc4190: false,
            extra: Default::default(),
        };
        self.registry
            .add(&reg)
            .expect("scenario registration must not collide");
    }
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}
