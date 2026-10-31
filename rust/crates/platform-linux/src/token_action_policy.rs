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
    use std::cell::Cell;

    #[test]
    fn observed_failure_never_retargets_under_either_window_proof() {
        for may_reresolve in [false, true] {
            let sibling_calls = Cell::new(0);
            let result: Result<(), _> = observed(|| Err("unavailable"));
            let _ = may_reresolve;
            assert_eq!(result, Err("unavailable"));
            assert_eq!(sibling_calls.get(), 0);
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
