use cua_driver_core::walk_budget::WalkBudget;

#[derive(Debug)]
pub(crate) struct WalkState<'a> {
    pub(crate) budget: &'a mut WalkBudget,
    max_depth: usize,
    pub(crate) complete: bool,
}

impl<'a> WalkState<'a> {
    pub(crate) fn new(budget: &'a mut WalkBudget, max_depth: usize) -> Self {
        Self {
            budget,
            max_depth,
            complete: true,
        }
    }

    /// Admit one node to the walk. Reaching a bound exactly is not itself
    /// truncation; completeness changes only when a node beyond that bound is
    /// actually encountered and skipped.
    pub(crate) fn enter(&mut self, depth: usize) -> bool {
        if depth > self.max_depth || !self.budget.admit() {
            self.complete = false;
            return false;
        }
        true
    }

    pub(crate) fn mark_incomplete(&mut self) {
        self.complete = false;
    }

    pub(crate) fn finish(self) -> bool {
        self.complete
    }
}

/// Offscreen deferred rows are walked only when a query searches them;
/// otherwise they stay a summary row, the element list misses them, and the
/// walk is not complete.
pub(crate) fn note_unwalked_offscreen_rows(
    walk_state: &mut WalkState<'_>,
    visible: impl IntoIterator<Item = bool>,
    search_offscreen: bool,
) {
    if !search_offscreen && visible.into_iter().any(|visible| !visible) {
        walk_state.mark_incomplete();
    }
}

#[cfg(test)]
#[path = "walk_completeness_tests.rs"]
mod tests;
