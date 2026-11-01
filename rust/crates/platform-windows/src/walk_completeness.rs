use std::cell::Cell;

use cua_driver_contract::{
    IncompleteReason, IncompleteReasonDetail as Detail, IncompleteReasonKind as Kind,
};
use cua_driver_core::{snapshot_compaction::TabStripCompaction, walk_budget::WalkBudget};

#[derive(Clone, Copy, Debug)]
pub(crate) enum Omission {
    DepthLimit,
    CacheRequestIncomplete,
    ChildrenUnreadable,
    ProcessIdUnreadable,
    SubtreeCacheFailed,
    EmptyWrapperFallback,
    OffscreenRowsDeferred,
    TabsCompacted,
    OffscreenContentDropped,
    AttributeReadFailed,
    SiblingScanCapped,
}
impl Omission {
    pub(crate) fn reason(self) -> IncompleteReason {
        use Omission::*;
        let (kind, detail) = match self {
            DepthLimit => (Kind::Budget, Detail::DepthLimit),
            CacheRequestIncomplete | ProcessIdUnreadable | AttributeReadFailed => {
                (Kind::ProviderFailure, Detail::AttributeReadFailed)
            }
            ChildrenUnreadable => (Kind::ProviderFailure, Detail::ChildEnumerationFailed),
            SubtreeCacheFailed => (Kind::ProviderFailure, Detail::EnumerationErrorUnclassified),
            EmptyWrapperFallback => (Kind::ProviderFailure, Detail::ScopeMismatch),
            OffscreenRowsDeferred | OffscreenContentDropped => {
                (Kind::PolicyOmission, Detail::Offscreen)
            }
            TabsCompacted => (Kind::PolicyOmission, Detail::InactiveTab),
            SiblingScanCapped => (Kind::PolicyOmission, Detail::HugeContainer),
        };
        IncompleteReason { kind, detail }
    }
}

#[derive(Debug, Default)]
pub(crate) struct ReadLog(Cell<u32>);
impl ReadLog {
    pub(crate) fn fail(&self) {
        self.0.set(self.0.get().saturating_add(1));
    }
    pub(crate) fn failed(&self) -> bool {
        self.0.get() != 0
    }
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CachedRead {
    Value,
    Absent,
    Failed,
}
#[derive(Debug)]
pub(crate) enum RawCached {
    Err(i32),
    Ok {
        vt: u16,
        unknown_is_not_supported_token: bool,
    },
}
pub(crate) fn classify_cached_variant(raw: RawCached, expected_vt: u16) -> CachedRead {
    match raw {
        RawCached::Ok { vt, .. } if vt == expected_vt => CachedRead::Value,
        RawCached::Ok {
            vt: 13,
            unknown_is_not_supported_token: true,
        } => CachedRead::Absent,
        RawCached::Err(code) => {
            let _ = code;
            CachedRead::Failed
        }
        _ => CachedRead::Failed,
    }
}
pub(crate) fn classify_cached_pattern(result: Result<(), i32>) -> CachedRead {
    match result {
        Ok(()) => CachedRead::Value,
        Err(code) if crate::uia_mutation::pattern_absent(code) => CachedRead::Absent,
        Err(_) => CachedRead::Failed,
    }
}

pub(crate) fn record_pattern_read(result: Result<(), i32>, log: &ReadLog) -> CachedRead {
    let classified = classify_cached_pattern(result);
    if classified == CachedRead::Failed {
        log.fail();
    }
    classified
}

#[derive(Debug)]
pub(crate) struct WalkState<'a> {
    pub(crate) budget: &'a mut WalkBudget,
    max_depth: usize,
    pub(crate) complete: bool,
    pub(crate) reads: ReadLog,
    #[cfg(target_os = "windows")]
    pub(crate) not_supported: Option<windows::core::IUnknown>,
}
impl<'a> WalkState<'a> {
    pub(crate) fn new(budget: &'a mut WalkBudget, max_depth: usize) -> Self {
        Self {
            budget,
            max_depth,
            complete: true,
            reads: ReadLog::default(),
            #[cfg(target_os = "windows")]
            not_supported: None,
        }
    }
    pub(crate) fn enter(&mut self, depth: usize) -> bool {
        if depth > self.max_depth {
            self.mark(Omission::DepthLimit);
            return false;
        }
        if !self.budget.admit() {
            self.complete = false;
            return false;
        }
        true
    }
    pub(crate) fn mark(&mut self, omission: Omission) {
        if !matches!(omission, Omission::OffscreenContentDropped) {
            self.complete = false;
        }
        if matches!(omission, Omission::DepthLimit | Omission::TabsCompacted) {
            self.budget.record_cut(omission.reason());
        } else {
            self.budget.record_skip(omission.reason());
        }
    }
    pub(crate) fn finish(mut self) -> bool {
        if self.reads.failed() {
            self.mark(Omission::AttributeReadFailed);
        }
        if self.complete {
            self.budget.assert_exhaustive();
        }
        self.complete
    }
}
pub(crate) fn note_unwalked_offscreen_rows(
    state: &mut WalkState<'_>,
    visible: impl IntoIterator<Item = bool>,
    search_offscreen: bool,
) {
    if !search_offscreen && visible.into_iter().any(|visible| !visible) {
        state.mark(Omission::OffscreenRowsDeferred);
    }
}
pub(crate) fn note_tab_plan(state: &mut WalkState<'_>, plan: Option<&TabStripCompaction>) {
    if plan.is_some_and(|p| p.omitted_count > 0) {
        state.mark(Omission::TabsCompacted);
    }
}
#[cfg(test)]
#[path = "walk_completeness_tests.rs"]
mod tests;
