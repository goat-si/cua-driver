use super::{Fence, ProcessStamp};

impl Fence {
    pub(super) fn record_outcome(
        &self,
        pid: i32,
        stamp: Option<ProcessStamp>,
        prior: Option<i32>,
        status: &str,
        reason: Option<&str>,
    ) {
        let Some(stamp) = stamp else {
            tracing::debug!(pid, "cannot bind restoration outcome to process");
            return;
        };
        let mut note = serde_json::json!({"status": status, "prior_pid": prior});
        if let Some(reason) = reason {
            note["reason"] = reason.into();
        }
        self.outcomes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(pid)
            .or_default()
            .push_back((stamp, note));
    }
    pub(super) fn take_outcome(&self, pid: i32) -> Option<serde_json::Value> {
        if !self
            .outcomes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&pid)
        {
            return None;
        }
        self.take_outcome_for(pid, (self.identity)(pid))
    }
    pub(super) fn take_outcome_for(
        &self,
        pid: i32,
        current: Option<ProcessStamp>,
    ) -> Option<serde_json::Value> {
        let mut outcomes = self.outcomes.lock().unwrap_or_else(|e| e.into_inner());
        let queue = outcomes.get_mut(&pid)?;
        queue.retain(|(stamp, _)| Some(*stamp) == current && current.is_some());
        let note = queue.pop_front().map(|(_, note)| note);
        if queue.is_empty() {
            outcomes.remove(&pid);
        }
        note
    }
    pub(super) fn record_chain_outcome(
        &self,
        root: i32,
        epoch: u64,
        prior: Option<i32>,
        status: &str,
        reason: Option<&str>,
    ) {
        let identity = {
            let mut obligations = self.obligations.lock().unwrap_or_else(|e| e.into_inner());
            let Some(chain) = obligations
                .get_mut(&root)
                .filter(|chain| chain.epoch == epoch)
            else {
                return;
            };
            let Some((pid, _, stamp, _)) = chain
                .targets
                .iter()
                .max_by_key(|(_, _, _, event_epoch)| *event_epoch)
                .copied()
            else {
                return;
            };
            chain.notified.insert((pid, stamp)).then_some((pid, stamp))
        };
        if let Some((pid, stamp)) = identity {
            self.record_outcome(pid, stamp, prior, status, reason);
        }
    }
}
