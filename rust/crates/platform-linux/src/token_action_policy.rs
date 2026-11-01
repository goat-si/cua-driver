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
    Partial,
    Unknown,
}

pub(crate) fn write_failure_result(failure: WriteFailure) -> Option<(&'static str, &'static str)> {
    match failure {
        WriteFailure::NoRoute => None,
        WriteFailure::Gone => Some(("stale_element_token", "none")),
        WriteFailure::BeforeDispatch => Some(("token_route_unavailable", "none")),
        WriteFailure::Partial => Some(("element_write_partial", "unverifiable")),
        WriteFailure::Unknown => Some(("element_write_unacknowledged", "unverifiable")),
    }
}

/// A text write that must end the tool call: nothing after it may write again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextWriteStop {
    /// A mutator was sent and its outcome is unknown.
    Uncertain,
    /// The field was cleared and the new text was not confirmed.
    Partial,
    /// The caller already stopped waiting and closed the dispatch gate.
    Closed,
}

/// (code, effect, note) for a stopped text write.
pub(crate) fn text_write_stop_result(stop: TextWriteStop) -> (&'static str, &'static str, &'static str) {
    match stop {
        TextWriteStop::Partial => (
            "element_write_partial",
            "unverifiable",
            "the field may be partly changed (its text was removed before the new text was confirmed); read it back before retrying",
        ),
        TextWriteStop::Uncertain | TextWriteStop::Closed => (
            "element_write_unacknowledged",
            "unverifiable",
            "the write may have run; read the field back before retrying",
        ),
    }
}

/// Outcome of the focus-free AT-SPI attempts of an untargeted type_text.
#[derive(Debug)]
pub(crate) enum UntargetedAx<E> {
    Written(&'static str),
    Stopped(E),
    NotWritten,
}

/// The first focus-free write, then the Qt5 synthetic-focus write. A stage that
/// may have changed the field ends the chain; only a stage that changed
/// nothing lets the next one run.
pub(crate) fn untargeted_ax_stages<E>(
    classify: impl Fn(&E) -> Option<TextWriteStop>,
    first: impl FnOnce() -> Result<(), E>,
    qt5: impl FnOnce() -> Result<(), E>,
) -> UntargetedAx<E> {
    match first() {
        Ok(()) => return UntargetedAx::Written("via AT-SPI"),
        Err(error) if classify(&error).is_some() => return UntargetedAx::Stopped(error),
        Err(_) => {}
    }
    match qt5() {
        Ok(()) => UntargetedAx::Written("via AT-SPI with focus workaround"),
        Err(error) if classify(&error).is_some() => UntargetedAx::Stopped(error),
        Err(_) => UntargetedAx::NotWritten,
    }
}

/// Combine the Qt5 write with its FocusOut restoration. The write decides:
/// an acknowledged write stays written even when FocusOut fails, and a write
/// that may have run is reported as such; only a write that changed nothing
/// reports the FocusOut failure.
pub(crate) fn qt5_restore_outcome<E>(
    write: Result<(), E>,
    focus_out: Result<(), E>,
    classify: impl Fn(&E) -> Option<TextWriteStop>,
) -> Result<(), E> {
    match write {
        Ok(()) => Ok(()),
        Err(error) if classify(&error).is_some() => Err(error),
        Err(error) => focus_out.and(Err(error)),
    }
}

/// The final stage's AT-SPI insert: `Ok(true)` written, `Ok(false)` nothing
/// changed so the key routes may run, `Err` the write may have run.
pub(crate) fn final_insert_step<E>(
    insert: Result<bool, E>,
    classify: impl Fn(&E) -> Option<TextWriteStop>,
) -> Result<bool, E> {
    match insert {
        Ok(written) => Ok(written),
        Err(error) if classify(&error).is_some() => Err(error),
        Err(_) => Ok(false),
    }
}

#[cfg(test)]
mod write_tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    enum E {
        Plain,
        Stop(TextWriteStop),
    }
    fn classify(error: &E) -> Option<TextWriteStop> {
        match error {
            E::Plain => None,
            E::Stop(stop) => Some(*stop),
        }
    }

    #[test]
    fn stopped_text_writes_are_unverifiable() {
        assert_eq!(
            write_failure_result(WriteFailure::Partial),
            Some(("element_write_partial", "unverifiable"))
        );
        for stop in [TextWriteStop::Uncertain, TextWriteStop::Partial, TextWriteStop::Closed] {
            let (code, effect, note) = text_write_stop_result(stop);
            assert_eq!(effect, "unverifiable");
            assert_eq!(
                code,
                if stop == TextWriteStop::Partial {
                    "element_write_partial"
                } else {
                    "element_write_unacknowledged"
                }
            );
            assert!(note.contains("before retrying"));
        }
    }

    #[test]
    fn untargeted_stages_stop_after_a_possible_write() {
        for stop in [TextWriteStop::Uncertain, TextWriteStop::Partial, TextWriteStop::Closed] {
            let mut qt5_calls = 0;
            let outcome = untargeted_ax_stages(classify, || Err(E::Stop(stop)), || {
                qt5_calls += 1;
                Ok(())
            });
            assert!(matches!(outcome, UntargetedAx::Stopped(E::Stop(s)) if s == stop));
            assert_eq!(qt5_calls, 0, "{stop:?}: Qt5 stage wrote after a possible write");

            let outcome = untargeted_ax_stages(classify, || Err(E::Plain), || Err(E::Stop(stop)));
            assert!(matches!(outcome, UntargetedAx::Stopped(E::Stop(s)) if s == stop));
        }
    }

    #[test]
    fn untargeted_stages_continue_only_when_nothing_changed() {
        let mut qt5_calls = 0;
        let outcome = untargeted_ax_stages(classify, || Ok(()), || {
            qt5_calls += 1;
            Ok(())
        });
        assert!(matches!(outcome, UntargetedAx::Written("via AT-SPI")));
        assert_eq!(qt5_calls, 0);
        let outcome = untargeted_ax_stages(classify, || Err(E::Plain), || Ok(()));
        assert!(matches!(
            outcome,
            UntargetedAx::Written("via AT-SPI with focus workaround")
        ));
        let outcome = untargeted_ax_stages(classify, || Err(E::Plain), || Err(E::Plain));
        assert!(matches!(outcome, UntargetedAx::NotWritten));
    }

    #[test]
    fn qt5_focus_out_failure_never_hides_the_write() {
        // An acknowledged write stays written: reporting the FocusOut error
        // would let the final stage type the text a second time.
        assert_eq!(qt5_restore_outcome(Ok(()), Err(E::Plain), classify), Ok(()));
        let stop = E::Stop(TextWriteStop::Uncertain);
        assert_eq!(
            qt5_restore_outcome(Err(stop), Err(E::Plain), classify),
            Err(E::Stop(TextWriteStop::Uncertain))
        );
        assert_eq!(qt5_restore_outcome(Err(E::Plain), Ok(()), classify), Err(E::Plain));
        assert_eq!(qt5_restore_outcome::<E>(Ok(()), Ok(()), classify), Ok(()));
    }

    #[test]
    fn final_insert_stops_only_on_a_possible_write() {
        assert_eq!(final_insert_step::<E>(Ok(true), classify), Ok(true));
        assert_eq!(final_insert_step::<E>(Ok(false), classify), Ok(false));
        assert_eq!(final_insert_step(Err(E::Plain), classify), Ok(false));
        for stop in [TextWriteStop::Uncertain, TextWriteStop::Partial, TextWriteStop::Closed] {
            assert_eq!(final_insert_step(Err(E::Stop(stop)), classify), Err(E::Stop(stop)));
        }
    }

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
