//! The foreman journal: the pure format lives in `pir-builtin-mode`.
//!
//! The crate owns the on-disk shape (`Event`/`JournalState`/`ForemanJournal`,
//! replay, atomic writes, compaction) because none of it needs the engine. This
//! module re-exports that surface under the historical
//! `super::journal::…` paths and keeps the ONE engine-side piece: the
//! `ModeState` journal mutators, which read and write the state the plugin
//! holds.

pub use pir_builtin_mode::journal::*;

use super::ModeState;

impl ModeState {
    pub(super) fn journal_append(&self, event: super::journal::JournalEvent, sync: bool) {
        let mut guard = self
            .journal
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(journal) = guard.as_mut() else {
            return;
        };
        if let Err(error) = journal.append(event, sync) {
            crate::ext_diag!("[mode] failed to append foreman journal: {error}");
        }
    }

    pub(super) fn journal_snapshot(&self, sync: bool) {
        let state = self.journal_state();
        let mut guard = self
            .journal
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(journal) = guard.as_mut() else {
            return;
        };
        if let Err(error) = journal.write_snapshot(&state, sync) {
            crate::ext_diag!("[mode] failed to write foreman journal snapshot: {error}");
        }
    }

    pub(super) fn journal_maybe_compact(&self) {
        let state = self.journal_state();
        let mut guard = self
            .journal
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(journal) = guard.as_mut() else {
            return;
        };
        if let Err(error) = journal.maybe_compact(&state) {
            crate::ext_diag!("[mode] failed to compact foreman journal: {error}");
        }
    }
}
