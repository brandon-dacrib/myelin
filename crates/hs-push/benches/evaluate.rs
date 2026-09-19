//! Push evaluation throughput: the two costs that sit on the hot path of every message sent to a
//! busy room, measured separately so the cache's claim in `hs_push::compiled`'s module docs is a
//! number rather than an assertion.
//!
//! Run with `cargo bench -p hs-push --bench evaluate`.
//!
//! - `evaluate/default_ruleset`: one `hs_push::engine::evaluate` call against the spec's
//!   predefined ruleset for one recipient. This is the irreducible per-recipient cost; a room with
//!   N joined local members pays it N times per event.
//! - `effective_ruleset/warm` vs `effective_ruleset/cold`: the cached versus uncached ruleset
//!   fetch. `cold` pays a store read plus a JSON deserialize of the whole ruleset; `warm` is a
//!   `HashMap` hit plus an `Arc` clone. The ratio between them is what `hs_push::compiled` exists
//!   to buy, and the reason the room-update consumer must go through
//!   `CachedRulesetStore::effective_ruleset` rather than the bare store.
//!
//! `engine::evaluate` and `effective_ruleset` are both `async` (Ruma's `AnyPushRuleRef::applies`
//! is), so each iteration blocks on a current-thread runtime built once outside the measured
//! closure. Criterion 0.5 is used without its `async_tokio` feature to avoid adding a workspace
//! feature for one benchmark.

use criterion::{Criterion, criterion_group, criterion_main};
use hs_push::compiled::RuleCache;
use hs_push::engine::{evaluate, flatten_event};
use hs_push::rulesets::{
    CachedRulesetStore, RulesetStore, default_ruleset, memory::InMemoryRulesetStore,
};
use ruma::push::{FlattenedJson, PushConditionRoomCtx};
use ruma::{UInt, room_id, user_id};

fn message_event() -> FlattenedJson {
    let json = serde_json::json!({
        "type": "m.room.message",
        "sender": "@bob:example.org",
        "room_id": "!bench:example.org",
        "content": {"msgtype": "m.text", "body": "the quick brown fox jumps over the lazy dog"},
    });
    flatten_event(&json.to_string()).expect("fixture is valid JSON")
}

fn ctx(user: &ruma::UserId) -> PushConditionRoomCtx {
    PushConditionRoomCtx::new(
        room_id!("!bench:example.org").to_owned(),
        UInt::from(1000u32),
        user.to_owned(),
        "Alice".to_owned(),
    )
}

fn bench_evaluate(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime");
    let alice = user_id!("@alice:example.org");
    let ruleset = default_ruleset(alice);
    let event = message_event();
    let context = ctx(alice);

    c.bench_function("evaluate/default_ruleset", |b| {
        b.iter(|| {
            let outcome = runtime.block_on(evaluate(&ruleset, &event, &context));
            std::hint::black_box(outcome)
        });
    });
}

fn bench_effective_ruleset(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime");
    let alice = user_id!("@alice:example.org");

    let store = InMemoryRulesetStore::new();
    runtime
        .block_on(store.set_ruleset(alice, &default_ruleset(alice)))
        .expect("in-memory write cannot fail");
    let cached = CachedRulesetStore::new(store);

    // Warm: the entry stays cached across iterations, so every iteration is a cache hit.
    runtime
        .block_on(cached.effective_ruleset(alice))
        .expect("warming read");
    c.bench_function("effective_ruleset/warm", |b| {
        b.iter(|| {
            let ruleset = runtime
                .block_on(cached.effective_ruleset(alice))
                .expect("cached read");
            std::hint::black_box(ruleset)
        });
    });

    // Cold: invalidate before each iteration so every one pays the store read and the deserialize.
    // The invalidation itself is a `HashMap::remove`, orders of magnitude below what it precedes.
    let cache: std::sync::Arc<RuleCache> = cached.cache();
    c.bench_function("effective_ruleset/cold", |b| {
        b.iter(|| {
            cache.invalidate(alice);
            let ruleset = runtime
                .block_on(cached.effective_ruleset(alice))
                .expect("uncached read");
            std::hint::black_box(ruleset)
        });
    });
}

criterion_group!(benches, bench_evaluate, bench_effective_ruleset);
criterion_main!(benches);
