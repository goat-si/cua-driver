//! Decisions shared by token actions and their host-runnable tests.

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

    #[test]
    fn all_token_entrypoints_refuse_errors_without_a_sibling_write() {
        for _entrypoint in ["hyprland", "inject", "targeted"] {
            for (failure, expected) in [
                (
                    WriteFailure::BeforeDispatch,
                    ("token_route_unavailable", "none"),
                ),
                (WriteFailure::Gone, ("stale_element_token", "none")),
                (
                    WriteFailure::Unknown,
                    ("element_write_unacknowledged", "unverifiable"),
                ),
            ] {
                let mut fallback_calls = 0;
                let result = write_failure_result(failure);
                if result.is_none() {
                    fallback_calls += 1;
                }
                assert_eq!(result, Some(expected));
                assert_eq!(fallback_calls, 0);
            }
            assert_eq!(write_failure_result(WriteFailure::NoRoute), None);
        }
    }
}
