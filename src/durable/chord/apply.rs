//! Trusted immutable application: the port of upstream
//! `packages/chord/src/delta/apply-immutable-trusted.ts`.
//!
//! This is the applier for a batch produced by [`diff_revisions`] in this
//! crate. The batch is trusted, so — exactly as upstream — nothing beyond what
//! is needed to resolve the path is validated, and the previous revision is
//! never mutated.
//!
//! [`diff_revisions`]: super::diff_revisions

use crate::durable::delta::{apply, DeltaError, Op};
use serde_json::Value;

/// Apply self-produced trusted operations, returning the next revision.
///
/// Upstream copies each container on the touched path once and shares every
/// untouched subtree. Rust's [`Value`] is an owned tree with no structural
/// sharing, so the same contract is met by taking the target by value and
/// applying each operation in place: the caller's previous revision has been
/// consumed rather than mutated through a shared reference, no intermediate
/// revision escapes, and the returned value is the one upstream produces.
///
/// Invalid operation shapes are reported rather than applied: a `p`/`m` whose
/// target is not an array, a non-numeric deletion key on an array, and an `a`/
/// `t` whose target is not a string are all errors, as they are upstream.
pub fn apply_immutable_trusted(target: Value, operations: &[Op]) -> Result<Value, DeltaError> {
    let mut root = target;
    for operation in operations {
        apply(&mut root, std::slice::from_ref(operation))?;
    }
    Ok(root)
}
