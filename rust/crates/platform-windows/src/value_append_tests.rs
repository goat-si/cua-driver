use super::*;
use std::cell::{Cell, RefCell};

#[test]
fn routing_table_observes_calls_and_results() {
    for (password, pattern, read_ok, write_ok, expected, reads, writes, written) in [
        (
            None,
            true,
            true,
            true,
            AppendOutcome::Refused("secure_state_unknown"),
            0,
            0,
            "",
        ),
        (
            None,
            false,
            true,
            true,
            AppendOutcome::Refused("secure_state_unknown"),
            0,
            0,
            "",
        ),
        (
            Some(true),
            false,
            true,
            true,
            AppendOutcome::Refused("secure_no_value_pattern"),
            0,
            0,
            "",
        ),
        (
            Some(true),
            true,
            true,
            true,
            AppendOutcome::ReplacedSecure,
            0,
            1,
            "d",
        ),
        (
            Some(true),
            true,
            true,
            false,
            AppendOutcome::SecureWriteFailed { hresult: -7 },
            0,
            1,
            "d",
        ),
        (
            Some(false),
            false,
            true,
            true,
            AppendOutcome::NoValuePattern,
            0,
            0,
            "",
        ),
        (
            Some(false),
            true,
            false,
            true,
            AppendOutcome::Refused("value_unreadable"),
            1,
            0,
            "",
        ),
        (
            Some(false),
            true,
            true,
            true,
            AppendOutcome::Written {
                before: "abc".into(),
                inserted: "abcd".into(),
            },
            1,
            1,
            "abcd",
        ),
        (
            Some(false),
            true,
            true,
            false,
            AppendOutcome::WriteFailed { hresult: -7 },
            1,
            1,
            "abcd",
        ),
    ] {
        let events = RefCell::new(Vec::new());
        let write_text = RefCell::new(String::new());
        let result = run_value_append(
            || {
                events.borrow_mut().push("secure");
                password
            },
            || {
                events.borrow_mut().push("acquire");
                Ok(pattern.then_some(()))
            },
            |_| {
                events.borrow_mut().push("read");
                if read_ok {
                    Ok("abc".to_owned())
                } else {
                    Err(-1)
                }
            },
            |_, text| {
                events.borrow_mut().push("write");
                *write_text.borrow_mut() = text.to_owned();
                if write_ok {
                    Ok(())
                } else {
                    Err(-7)
                }
            },
            "d",
        );
        assert_eq!(result, expected);
        let events = events.into_inner();
        assert_eq!(&events[..2], &["secure", "acquire"]);
        assert_eq!(events.iter().filter(|&&x| x == "acquire").count(), 1);
        assert_eq!(events.iter().filter(|&&x| x == "read").count(), reads);
        assert_eq!(events.iter().filter(|&&x| x == "write").count(), writes);
        assert_eq!(*write_text.borrow(), written);
        assert_eq!(
            allows_wm_char_fallback(&result),
            matches!(result, AppendOutcome::NoValuePattern)
        );
        let expected_route = if allows_wm_char_fallback(&result) {
            Route::WmChar
        } else {
            Route::Continue(result.clone())
        };
        assert_eq!(route_after_worker(Ok(Some(result))), expected_route);
    }
}

#[test]
fn uia_mutation_write_failure_only_falls_back_when_not_supported() {
    for (hresult, expected) in [
        (0x8013_1505_u32 as i32, false),
        (0x8004_0204_u32 as i32, false),
    ] {
        let outcome = AppendOutcome::WriteFailed { hresult };
        assert_eq!(allows_wm_char_fallback(&outcome), expected);
        assert_eq!(
            route_after_worker(Ok(Some(outcome.clone()))),
            if expected {
                Route::WmChar
            } else {
                Route::Continue(outcome)
            }
        );
    }
}

#[test]
fn worker_failure_is_terminal() {
    assert_eq!(route_after_worker(Err(WorkerFailed)), Route::Terminal);
}

#[test]
fn non_uia_indexed_element_keeps_the_typing_route() {
    // The worker returns None only when the admitted element is not a UIA
    // element (an MSAA row, e.g. VCL/LibreOffice). Nothing was written, and
    // before this change those elements were typed into with WM_CHAR.
    assert_eq!(route_after_worker(Ok(None)), Route::WmChar);
}

#[test]
fn gated_reads_never_touch_secure_or_unknown() {
    for password in [Some(true), None, Some(false)] {
        let values = Cell::new(0);
        let texts = Cell::new(0);
        let result = read_value_gated(
            || password,
            || {
                values.set(values.get() + 1);
                None
            },
            || {
                texts.set(texts.get() + 1);
                Some("document".into())
            },
        );
        assert_eq!(
            result.as_deref(),
            if password == Some(false) {
                Some("document")
            } else {
                None
            }
        );
        assert_eq!(values.get(), usize::from(password == Some(false)));
        assert_eq!(texts.get(), usize::from(password == Some(false)));
    }
    let texts = Cell::new(0);
    assert_eq!(
        read_value_gated(
            || Some(false),
            || Some("value".into()),
            || {
                texts.set(1);
                None
            }
        )
        .as_deref(),
        Some("value")
    );
    assert_eq!(texts.get(), 0);
}

#[test]
fn document_requires_each_condition_and_explicit_non_password() {
    assert!(focused_document_target_ok(
        true,
        true,
        true,
        true,
        Some(false),
        true
    ));
    for args in [
        (false, true, true, true, Some(false), true),
        (true, false, true, true, Some(false), true),
        (true, true, false, true, Some(false), true),
        (true, true, true, false, Some(false), true),
        (true, true, true, true, None, true),
        (true, true, true, true, Some(true), true),
        (true, true, true, true, Some(false), false),
    ] {
        assert!(!focused_document_target_ok(
            args.0, args.1, args.2, args.3, args.4, args.5
        ));
    }
}

#[test]
fn failed_value_pattern_acquisition_writes_nothing_and_never_types() {
    const TIMEOUT: i32 = 0x8013_1505_u32 as i32;
    for password in [Some(true), Some(false)] {
        let writes = std::cell::Cell::new(0);
        let reads = std::cell::Cell::new(0);
        let result = run_value_append(
            || password,
            || Err::<Option<()>, i32>(TIMEOUT),
            |_| {
                reads.set(reads.get() + 1);
                Ok(String::new())
            },
            |_, _| {
                writes.set(writes.get() + 1);
                Ok(())
            },
            "secret",
        );
        assert_eq!(result, AppendOutcome::Refused("value_pattern_unreadable"));
        assert_eq!((reads.get(), writes.get()), (0, 0));
        assert!(!allows_wm_char_fallback(&result));
    }
}
