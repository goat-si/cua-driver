//! Decisions shared by token actions and their host-runnable tests.

pub(crate) fn ax_click_structured(suspected_noop: bool, unacknowledged: bool) -> serde_json::Value {
    let mut result = serde_json::json!({"path": "ax", "verified": false,
        "effect": if suspected_noop && !unacknowledged { "suspected_noop" } else { "unverifiable" }});
    if unacknowledged {
        result["code"] = serde_json::json!("element_action_unacknowledged");
        result["note"] = serde_json::json!("no acknowledgement received for the doAction call; the action may have run. Re-snapshot before retrying");
    }
    result
}

pub(crate) fn observed<T, E>(action: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
    action()
}

pub(crate) fn proves_gone(name: Option<&str>) -> bool {
    matches!(
        name,
        Some(
            "org.freedesktop.DBus.Error.UnknownObject"
                | "org.freedesktop.DBus.Error.NameHasNoOwner"
                | "org.freedesktop.DBus.Error.ServiceUnknown"
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ax_click_unacknowledged_takes_precedence() {
        for suspected in [false, true] {
            for unacknowledged in [false, true] {
                let actual = ax_click_structured(suspected, unacknowledged);
                assert_eq!(actual["verified"], false);
                assert_eq!(
                    actual["effect"],
                    if unacknowledged || !suspected {
                        "unverifiable"
                    } else {
                        "suspected_noop"
                    }
                );
                assert_eq!(
                    actual.get("code").and_then(|v| v.as_str()),
                    unacknowledged.then_some("element_action_unacknowledged")
                );
                assert_eq!(actual.get("note").and_then(|v| v.as_str()), unacknowledged.then_some("no acknowledgement received for the doAction call; the action may have run. Re-snapshot before retrying"));
            }
        }
    }

    #[test]
    fn only_proven_object_or_owner_loss_is_gone() {
        for name in [
            "org.freedesktop.DBus.Error.UnknownObject",
            "org.freedesktop.DBus.Error.NameHasNoOwner",
            "org.freedesktop.DBus.Error.ServiceUnknown",
        ] {
            assert!(proves_gone(Some(name)), "{name}");
        }
        for name in [
            "org.freedesktop.DBus.Error.UnknownMethod",
            "org.freedesktop.DBus.Error.AccessDenied",
        ] {
            assert!(!proves_gone(Some(name)), "{name}");
        }
        assert!(!proves_gone(None));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActionFailure {
    BeforeDispatch,
    Unknown,
}

pub(crate) fn action_failure_result(failure: ActionFailure) -> (&'static str, &'static str) {
    match failure {
        ActionFailure::BeforeDispatch => ("element_bounds_unavailable", "none"),
        ActionFailure::Unknown => ("element_action_unacknowledged", "unverifiable"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriteFailure {
    NoRoute,
    Gone,
    BeforeDispatch,
    Unknown,
}

pub(crate) fn write_failure_result(failure: WriteFailure) -> Option<(&'static str, &'static str)> {
    match failure {
        WriteFailure::NoRoute => None,
        WriteFailure::Gone => Some(("stale_element_token", "none")),
        WriteFailure::BeforeDispatch => Some(("token_route_unavailable", "none")),
        WriteFailure::Unknown => Some(("element_write_unacknowledged", "unverifiable")),
    }
}

#[cfg(test)]
mod write_tests {
    use super::*;

    #[test]
    fn action_after_send_is_unverifiable() {
        assert_eq!(
            action_failure_result(ActionFailure::Unknown),
            ("element_action_unacknowledged", "unverifiable")
        );
        assert_eq!(
            action_failure_result(ActionFailure::BeforeDispatch),
            ("element_bounds_unavailable", "none")
        );
    }
}
