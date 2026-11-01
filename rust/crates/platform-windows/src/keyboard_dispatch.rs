use cua_driver_core::mutation::{Reason, WriteOutcome};
#[derive(Debug)]
pub(crate) struct InputDispatchFailure { pub inserted_any: bool, pub message: String }
impl std::fmt::Display for InputDispatchFailure { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.message) } }
impl std::error::Error for InputDispatchFailure {}
pub(crate) fn keyboard_failure_outcome(focus_uncertain: bool, dispatch: Option<bool>) -> Option<WriteOutcome> {
    if focus_uncertain || dispatch == Some(true) { Some(WriteOutcome::StopUncertain(Reason::Unknown)) }
    else if dispatch == Some(false) { Some(WriteOutcome::StopNotDelivered(Reason::UnconfirmedAlive)) } else { None }
}
pub(crate) fn key_batch_failure(sent: usize, total: usize, cleanup: impl std::fmt::Display) -> InputDispatchFailure {
    InputDispatchFailure { inserted_any: sent > 0, message: format!("SendInput inserted only {sent} of {total} events; possible causes: UIPI or a locked or secure input desktop. {cleanup}") }
}
pub(crate) enum UnitFailure { ForegroundLost { hwnd: String }, Short { sent: usize, cleanup: String }, Stalled }
pub(crate) fn text_unit_failure(k: usize, n: usize, failure: UnitFailure) -> InputDispatchFailure {
    let (inserted_any, message) = match failure {
        UnitFailure::ForegroundLost { hwnd } => (k > 0, format!("foreground_lost: foreground moved to HWND {hwnd} after {k} of {n} text units were read; the remaining {} were not sent. Nothing was retried; inspect the target before sending the rest.", n-k)),
        UnitFailure::Short { sent, cleanup } => (k > 0 || sent > 0, format!("SendInput inserted only {sent} of 4 key events for text unit {} of {n} ({k} earlier units were read). Possible causes: UIPI or a locked or secure input desktop. {cleanup}", k+1)),
        UnitFailure::Stalled => (true, format!("text_delivery_stalled: the target did not read text unit {} of {n} within 2000 ms; {k} earlier units were read. Unit {} was inserted and may still appear, possibly in the previously foreground window. Nothing was retried; inspect the target before sending the rest.",k+1,k+1)),
    }; InputDispatchFailure { inserted_any, message }
}
pub(crate) fn post_sequence<M>(msgs: &[M], mut post: impl FnMut(&M) -> anyhow::Result<()>) -> anyhow::Result<()> {
    for (i, msg) in msgs.iter().enumerate() { if let Err(e) = post(msg) { return Err(InputDispatchFailure { inserted_any: i > 0, message: e.to_string() }.into()); } } Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn key_batch_failure_carries_insertion_state() {
        let err: anyhow::Error = key_batch_failure(1, 2, "cleanup").into();
        assert!(err.downcast_ref::<InputDispatchFailure>().is_some(), "missing typed dispatch failure");
    }
}
