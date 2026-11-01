use cua_driver_core::mutation::{Reason, WriteOutcome};
#[derive(Debug)]
pub(crate) struct InputDispatchFailure {
    pub inserted_any: bool,
    pub message: String,
}
impl std::fmt::Display for InputDispatchFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for InputDispatchFailure {}
pub(crate) fn keyboard_failure_outcome(
    focus_uncertain: bool,
    dispatch: Option<bool>,
) -> Option<WriteOutcome> {
    if focus_uncertain || dispatch == Some(true) {
        Some(WriteOutcome::StopUncertain(Reason::Unknown))
    } else if dispatch == Some(false) {
        Some(WriteOutcome::StopNotDelivered(Reason::UnconfirmedAlive))
    } else {
        None
    }
}
pub(crate) fn key_batch_failure(
    sent: usize,
    total: usize,
    cleanup: impl std::fmt::Display,
) -> InputDispatchFailure {
    InputDispatchFailure { inserted_any: sent > 0, message: format!("SendInput inserted only {sent} of {total} events; possible causes: UIPI or a locked or secure input desktop. {cleanup}") }
}
pub(crate) enum UnitFailure {
    ForegroundLost { hwnd: String },
    Short { sent: usize, cleanup: String },
    Stalled,
}
pub(crate) fn text_unit_failure(k: usize, n: usize, failure: UnitFailure) -> InputDispatchFailure {
    let (inserted_any, message) = match failure {
        UnitFailure::ForegroundLost { hwnd } => (k > 0, format!("foreground_lost: foreground moved to HWND {hwnd} after {k} of {n} text units were read; the remaining {} were not sent. Nothing was retried; inspect the target before sending the rest.", n-k)),
        UnitFailure::Short { sent, cleanup } => (k > 0 || sent > 0, format!("SendInput inserted only {sent} of 4 key events for text unit {} of {n} ({k} earlier units were read). Possible causes: UIPI or a locked or secure input desktop. {cleanup}", k+1)),
        UnitFailure::Stalled => (true, format!("text_delivery_stalled: the target did not read text unit {} of {n} within 2000 ms; {k} earlier units were read. Unit {} was inserted and may still appear, possibly in the previously foreground window. Nothing was retried; inspect the target before sending the rest.",k+1,k+1)),
    };
    InputDispatchFailure {
        inserted_any,
        message,
    }
}
pub(crate) fn post_sequence<M>(
    msgs: &[M],
    mut post: impl FnMut(&M) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    for (i, msg) in msgs.iter().enumerate() {
        if let Err(e) = post(msg) {
            return Err(InputDispatchFailure {
                inserted_any: i > 0,
                message: e.to_string(),
            }
            .into());
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn key_batch_failure_carries_insertion_state() {
        let err: anyhow::Error = key_batch_failure(1, 2, "cleanup").into();
        assert!(
            err.downcast_ref::<InputDispatchFailure>().is_some(),
            "missing typed dispatch failure"
        );
    }
    fn check(error: anyhow::Error, expected: bool) {
        let failure = error.downcast_ref::<InputDispatchFailure>().unwrap();
        assert_eq!(failure.inserted_any, expected);
        let outcome = keyboard_failure_outcome(false, Some(failure.inserted_any)).unwrap();
        for tool in ["type_text", "press_key", "hotkey"] {
            let result = crate::uia_mutation::chain_error(
                error.to_string(),
                serde_json::json!({"path":"key_events", "verified":false}),
                [outcome],
                true,
            );
            let wire = cua_driver_core::mcp_result::conforming_tool_result(
                tool,
                serde_json::to_value(result).unwrap(),
            );
            assert_eq!(wire["structuredContent"]["effect"], "unverifiable");
            assert_eq!(
                serde_json::from_value::<cua_driver_contract::ErrorDisposition>(
                    wire["structuredContent"]["disposition"].clone()
                )
                .unwrap(),
                cua_driver_contract::ErrorDisposition::SUSPECTED_NOOP
            );
        }
    }
    #[test]
    fn producer_states_and_dispositions() {
        for (sent, expected) in [(0, false), (1, true)] {
            check(key_batch_failure(sent, 2, "cleanup").into(), expected);
        }
        for (k, sent, expected) in [(0, 0, false), (0, 1, true), (1, 0, true)] {
            check(
                text_unit_failure(
                    k,
                    2,
                    UnitFailure::Short {
                        sent,
                        cleanup: "cleanup".into(),
                    },
                )
                .into(),
                expected,
            );
        }
        for (k, expected) in [(0, false), (1, true)] {
            check(
                text_unit_failure(k, 2, UnitFailure::ForegroundLost { hwnd: "0".into() }).into(),
                expected,
            );
        }
        for k in 0..2 {
            check(text_unit_failure(k, 2, UnitFailure::Stalled).into(), true);
        }
        for failed in 0..2 {
            let mut count = 0;
            let error = post_sequence(&[0, 1], |_| {
                let i = count;
                count += 1;
                if i == failed {
                    anyhow::bail!("post failed")
                }
                Ok(())
            })
            .unwrap_err();
            check(error, failed > 0);
        }
        assert!(keyboard_failure_outcome(false, None).is_none());
        assert!(matches!(
            keyboard_failure_outcome(true, None),
            Some(WriteOutcome::StopUncertain(Reason::Unknown))
        ));
    }
    #[tokio::test]
    async fn join_error_finalizes_uncertain() {
        let error = tokio::spawn(async { panic!("worker") }).await.unwrap_err();
        let result = crate::uia_mutation::chain_error(
            format!("Task error: {error}"),
            serde_json::json!({"path":"key_events", "verified":false}),
            [WriteOutcome::StopUncertain(Reason::Unknown)],
            true,
        );
        let wire = cua_driver_core::mcp_result::conforming_tool_result(
            "press_key",
            serde_json::to_value(result).unwrap(),
        );
        assert_eq!(
            serde_json::from_value::<cua_driver_contract::ErrorDisposition>(
                wire["structuredContent"]["disposition"].clone()
            )
            .unwrap(),
            cua_driver_contract::ErrorDisposition::SUSPECTED_NOOP
        );
    }
}
