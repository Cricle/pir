//! The built-in `pi.usage` document (spec §8.6).
//!
//! Port of upstream `packages/durable/src/harness/usage.ts`. `pi.usage` is the
//! ledger of one conversation's own spend:
//!
//! * `models` — assistant entries and compaction summarization attempts, keyed
//!   `provider/modelId`;
//! * `tools` — tool results, keyed by tool name (their usage has no model
//!   identity).
//!
//! Every built-in writer adds the usage in the same commit as the entry. Failed
//! and aborted attempts count. A fork starts at zero, so no spend is counted
//! twice.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

use crate::ai::types::{Cost, Usage};
use crate::durable::documents::{AnyDocDefinition, CommonDocDefinition};
use crate::durable::types::{DocumentFork, JsonObject};

/// The persisted document kind.
pub const USAGE_KIND: &str = "pi.usage";
/// The persisted document version.
pub const USAGE_VERSION: i64 = 1;

/// The two ledger buckets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UsageScope {
    /// Assistant entries and summarization attempts, keyed `provider/modelId`.
    Models,
    /// Tool results, keyed by tool name.
    Tools,
}

/// Ledger of one conversation's own spend.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageState {
    /// Assistant entries and summarization attempts, keyed `provider/modelId`.
    #[serde(default)]
    pub models: BTreeMap<String, Usage>,
    /// Tool results, keyed by tool name.
    #[serde(default)]
    pub tools: BTreeMap<String, Usage>,
}

impl UsageState {
    /// An empty ledger.
    pub fn new() -> Self {
        Self::default()
    }

    /// The bucket a scope names.
    pub fn bucket_mut(&mut self, scope: UsageScope) -> &mut BTreeMap<String, Usage> {
        match scope {
            UsageScope::Models => &mut self.models,
            UsageScope::Tools => &mut self.tools,
        }
    }

    /// The bucket a scope names, read-only.
    pub fn bucket(&self, scope: UsageScope) -> &BTreeMap<String, Usage> {
        match scope {
            UsageScope::Models => &self.models,
            UsageScope::Tools => &self.tools,
        }
    }
}

/// Add `usage` to one bucket of the conversation's ledger.
///
/// A key that is a tool's name (or a model's `provider/modelId`) is an ordinary
/// map key: a [`BTreeMap`] has no prototype, so a tool called `toString` or
/// `__proto__` is just another entry.
pub fn usage_add(state: &mut UsageState, scope: UsageScope, key: &str, usage: &Usage) {
    let bucket = state.bucket_mut(scope);
    match bucket.get_mut(key) {
        Some(total) => add_usage(total, usage),
        None => {
            bucket.insert(key.to_owned(), *usage);
        }
    }
}

/// Add every counter of `usage` to `total`.
///
/// Optional counters (`cacheWrite1h`, `reasoning`) are added once either side
/// reports them.
pub fn add_usage(total: &mut Usage, usage: &Usage) {
    total.input = total.input.saturating_add(usage.input);
    total.output = total.output.saturating_add(usage.output);
    total.cache_read = total.cache_read.saturating_add(usage.cache_read);
    total.cache_write = total.cache_write.saturating_add(usage.cache_write);
    total.total_tokens = total.total_tokens.saturating_add(usage.total_tokens);
    total.cache_write_1h = add_optional(total.cache_write_1h, usage.cache_write_1h);
    total.reasoning = add_optional(total.reasoning, usage.reasoning);
    add_cost(&mut total.cost, &usage.cost);
}

/// Add every bucket of `state` into `sum`.
pub fn add_usage_state(sum: &mut UsageState, state: &UsageState) {
    for (scope, bucket) in [
        (UsageScope::Models, &state.models),
        (UsageScope::Tools, &state.tools),
    ] {
        for (key, usage) in bucket {
            usage_add(sum, scope, key, usage);
        }
    }
}

fn add_optional(total: Option<u64>, value: Option<u64>) -> Option<u64> {
    match (total, value) {
        (Some(total), Some(value)) => Some(total.saturating_add(value)),
        (Some(total), None) => Some(total),
        (None, value) => value,
    }
}

fn add_cost(total: &mut Cost, cost: &Cost) {
    total.input += cost.input;
    total.output += cost.output;
    total.cache_read += cost.cache_read;
    total.cache_write += cost.cache_write;
    total.total += cost.total;
}

/// Initial value of a new `pi.usage` incarnation: `{ models: {}, tools: {} }`.
fn usage_initial() -> JsonObject {
    let mut value = JsonObject::new();
    value.insert("models".to_owned(), serde_json::json!({}));
    value.insert("tools".to_owned(), serde_json::json!({}));
    value
}

/// `pi.usage`'s checkpoint predicate: a complete base on every change.
fn usage_checkpoint_when(_value: &JsonObject) -> bool {
    true
}

/// The built-in `pi.usage` definition (spec §8.6).
///
/// Kind `pi.usage`, version 1, conversation scope, `history: "latest"`,
/// `fork: "initial"`, initial `{ models: {}, tools: {} }`, a complete base on
/// every change.
///
/// A `LazyLock` because the definition carries function pointers and building
/// it validates the version, which is not a `const` operation.
pub static USAGE_DEFINITION: LazyLock<AnyDocDefinition> = LazyLock::new(|| {
    AnyDocDefinition::define(
        CommonDocDefinition::conversation_latest(USAGE_KIND, USAGE_VERSION, DocumentFork::Initial)
            .expect("the pi.usage definition version is valid"),
        usage_initial,
        None,
        Some(usage_checkpoint_when),
    )
});
