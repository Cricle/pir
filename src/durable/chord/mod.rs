//! The `@earendil-works/chord/delta` subset the durable session kernel needs:
//! **revision diffing** and **trusted immutable application**, built on
//! [`crate::durable::delta`].
//!
//! # What is ported, and what is deliberately not
//!
//! Durable imports only the delta closure of chord (`Op`/`apply`, `overlap`,
//! the tracker and `apply-immutable-trusted`). This module covers the diffing
//! and trusted-apply halves; the tracker is replaced by a full-draft diff (see
//! `docs/durable-spec.md` §12.4).
//!
//! **Not ported, by decision:** chord's host / facet / object / reference /
//! link / remote / websocket / hot-reload / placement / registry layers and its
//! replicated-state protocol. They exist for multi-process, multi-tab and
//! edge-hosted deployments; a single-process harness needs none of them. The
//! in-memory observation contract durable *does* use is implemented natively in
//! [`crate::durable::session::observation`]. Re-open that decision only if pir
//! ever needs multi-host document replication.
//!
//! [`delta`](crate::durable::delta) already carries the operation vocabulary
//! (`Op`, `Seg`, `Path`) and a general applier. This module adds the two halves
//! of upstream's chord delta that only a writer of revisions needs:
//!
//! | module | upstream | contents |
//! |---|---|---|
//! | [`diff`] | `src/delta/diff.ts`, `overlap` from `src/delta/index.ts` | [`diff_revisions`], [`diff_objects`], [`overlap`] |
//! | [`apply`] | `src/delta/apply-immutable-trusted.ts` | [`apply_immutable_trusted`] |
//!
//! `diff_revisions` computes a compact batch from two immutable revisions and
//! `apply_immutable_trusted` replays a batch this crate produced without
//! mutating the previous value. Ops produced here are [`Op`](crate::durable::delta::Op)
//! values, so they serialize with the same tuple wire form the delta module
//! documents and can be replayed by [`apply`](crate::durable::delta::apply).

pub mod apply;
pub mod diff;

pub use apply::apply_immutable_trusted;
pub use diff::{diff_objects, diff_revisions, overlap};
