use super::*;
use cua_driver_core::actionability::ActionabilityReport;
use std::{
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
};

#[derive(Default)]
struct FakeClock(Mutex<Duration>);
impl Clock for FakeClock {
    fn now(&self) -> Duration {
        *self.0.lock().unwrap()
    }
    fn sleep(&self, d: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            *self.0.lock().unwrap() += d;
        })
    }
}
fn raw() -> RawLiveFacts {
    RawLiveFacts {
        is_offscreen: LiveRead::Value(false),
        is_enabled: LiveRead::Value(true),
        bounds: LiveRead::Value(Rect {
            x: 0.,
            y: 0.,
            width: 10.,
            height: 10.,
        }),
        window_visible: LiveRead::Value(true),
        window_minimized: LiveRead::Value(false),
        window_cloaked: LiveRead::Value(false),
        control_type: LiveRead::Value("Edit".into()),
        patterns: LiveRead::Value(vec![PatternKind::Value]),
        value_read_only: LiveRead::Value(false),
        range_read_only: LiveRead::NotSupported,
    }
}
fn evaluated(raw: &RawLiveFacts, kind: ValueWrite) -> ActionabilityReport {
    evaluate(
        Intent::SetValue,
        &element_facts(raw, kind),
        Policy::headless(),
    )
}
fn assert_transient(raw: &RawLiveFacts) {
    assert!(matches!(
        evaluated(raw, ValueWrite::Value).verdict,
        Verdict::Transient { .. }
    ));
}
#[test]
fn evidence_conversion_and_visibility() {
    assert_eq!(evidence(&LiveRead::Value(true)), Evidence::Known(true));
    assert_eq!(evidence::<bool>(&LiveRead::NotSupported), Evidence::Absent);
    assert_eq!(
        evidence::<bool>(&LiveRead::Failed("COM".into(), None)),
        Evidence::Unknown("COM".into())
    );
    assert_eq!(
        element_facts(&raw(), ValueWrite::Value).visible,
        Evidence::Known(true)
    );
    for mutate in [0, 1, 2, 3] {
        let mut f = raw();
        match mutate {
            0 => f.is_offscreen = LiveRead::Value(true),
            1 => f.window_visible = LiveRead::Value(false),
            2 => f.window_minimized = LiveRead::Value(true),
            _ => f.window_cloaked = LiveRead::Value(true),
        }
        assert_eq!(
            element_facts(&f, ValueWrite::Value).visible,
            Evidence::Known(false)
        );
        assert_transient(&f);
    }
    let mut f = raw();
    f.window_cloaked = LiveRead::Failed("dwm".into(), None);
    assert_eq!(
        element_facts(&f, ValueWrite::Value).visible,
        Evidence::Unknown("dwm".into())
    );
    assert_transient(&f);
    f.window_visible = LiveRead::Value(false);
    assert_eq!(
        element_facts(&f, ValueWrite::Value).visible,
        Evidence::Known(false)
    );
}
#[test]
fn editable_enabled_geometry_and_roles() {
    let mut f = raw();
    assert_eq!(evaluated(&f, ValueWrite::Value).verdict, Verdict::Pass);
    f.is_enabled = LiveRead::Value(false);
    assert_transient(&f);
    f.is_enabled = LiveRead::Failed("enabled".into(), None);
    assert_transient(&f);
    f.is_enabled = LiveRead::Value(true);
    f.value_read_only = LiveRead::Value(true);
    assert!(matches!(
        evaluated(&f, ValueWrite::Value).verdict,
        Verdict::Terminal {
            code: "unsupported_action",
            ..
        }
    ));
    f.value_read_only = LiveRead::Failed("readonly".into(), None);
    assert_transient(&f);
    f.value_read_only = LiveRead::NotSupported;
    f.patterns = LiveRead::Value(vec![]);
    assert_eq!(
        element_facts(&f, ValueWrite::Value).editable,
        Evidence::Absent
    );
    assert!(matches!(
        evaluated(&f, ValueWrite::Value).verdict,
        Verdict::Terminal { .. }
    ));
    f.patterns = LiveRead::Value(vec![PatternKind::RangeValue]);
    f.range_read_only = LiveRead::Value(true);
    assert!(matches!(
        evaluated(&f, ValueWrite::RangeValue).verdict,
        Verdict::Terminal { .. }
    ));
    f.range_read_only = LiveRead::Value(false);
    assert_eq!(evaluated(&f, ValueWrite::RangeValue).verdict, Verdict::Pass);
    f.bounds = LiveRead::Value(Rect {
        x: 0.,
        y: 0.,
        width: 0.,
        height: 1.,
    });
    assert!(matches!(
        evaluated(&f, ValueWrite::RangeValue).verdict,
        Verdict::Transient { .. }
    ));
    f.bounds = LiveRead::Value(Rect {
        x: 0.,
        y: 0.,
        width: -1.,
        height: 1.,
    });
    assert!(matches!(
        evaluated(&f, ValueWrite::RangeValue).verdict,
        Verdict::Transient { unknown: true, .. }
    ));
    f.patterns = LiveRead::Failed("scan".into(), None);
    assert_eq!(
        element_facts(&f, ValueWrite::RangeValue).affordances,
        Evidence::Unknown("scan".into())
    );
    for (role, expected) in [
        ("Edit", "textfield"),
        ("ComboBox", "combobox"),
        ("Slider", "slider"),
    ] {
        f.control_type = LiveRead::Value(role.into());
        assert_eq!(
            element_facts(&f, ValueWrite::Value).role,
            Evidence::Known(expected.into())
        );
    }
}
#[tokio::test]
async fn disabled_facts_never_write() {
    let clock = FakeClock::default();
    let writes = AtomicUsize::new(0);
    let mut f = raw();
    f.is_enabled = LiveRead::Value(false);
    let result = gate_then_write(
        &clock,
        Duration::from_millis(200),
        |_| std::future::ready(Probe::uia(f.clone())),
        |_| async {
            writes.fetch_add(1, Ordering::SeqCst);
        },
    )
    .await;
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    let refusal = result.unwrap_err();
    assert_eq!(
        refusal.structured_content.as_ref().unwrap()["code"],
        "element_not_actionable"
    );
    assert_eq!(
        refusal.disposition,
        Some(cua_driver_contract::ErrorDisposition::NOT_DELIVERED)
    );
    let wire = cua_driver_core::mcp_result::conforming_tool_result(
        "set_value",
        serde_json::to_value(refusal).unwrap(),
    );
    assert_eq!(wire["structuredContent"]["code"], "element_not_actionable");
    assert_eq!(
        serde_json::from_value::<cua_driver_contract::ErrorDisposition>(
            wire["structuredContent"]["disposition"].clone()
        )
        .unwrap(),
        cua_driver_contract::ErrorDisposition::NOT_DELIVERED
    );
}
#[tokio::test]
async fn passing_and_unknown_then_passing_write_once() {
    for unknown_first in [false, true] {
        let clock = FakeClock::default();
        let writes = AtomicUsize::new(0);
        let mut count = 0;
        let result = gate_then_write(
            &clock,
            Duration::from_secs(1),
            |_| {
                count += 1;
                let mut f = raw();
                if unknown_first && count == 1 {
                    f.is_enabled = LiveRead::Failed("busy".into(), None);
                }
                std::future::ready(Probe::uia(f))
            },
            |_| async {
                writes.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(writes.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test]
async fn readonly_pending_and_late_probe_refuse() {
    let clock = FakeClock::default();
    let writes = AtomicUsize::new(0);
    let mut f = raw();
    f.value_read_only = LiveRead::Value(true);
    assert!(gate_then_write(
        &clock,
        Duration::from_secs(1),
        |_| std::future::ready(Probe::uia(f.clone())),
        |_| async {
            writes.fetch_add(1, Ordering::SeqCst);
        }
    )
    .await
    .is_err());
    assert_eq!(clock.now(), Duration::ZERO);
    let clock = FakeClock::default();
    assert!(gate_then_write(
        &clock,
        Duration::from_millis(200),
        |_| std::future::pending::<Probe>(),
        |_| async {
            writes.fetch_add(1, Ordering::SeqCst);
        }
    )
    .await
    .is_err());
    let clock = FakeClock::default();
    assert!(gate_then_write(
        &clock,
        Duration::from_millis(200),
        |_| {
            *clock.0.lock().unwrap() += Duration::from_millis(201);
            std::future::ready(Probe::uia(raw()))
        },
        |_| async {
            writes.fetch_add(1, Ordering::SeqCst);
        }
    )
    .await
    .is_err());
    assert_eq!(writes.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn admission_and_route_changes() {
    let mut f = raw();
    f.patterns = LiveRead::Value(vec![PatternKind::Value, PatternKind::RangeValue]);
    f.range_read_only = LiveRead::Value(true);
    let a = admission(&f);
    assert!(may_write(Rung::Value, &a));
    assert!(!may_write(Rung::RangeValue, &a));
    f.range_read_only = LiveRead::Value(false);
    let a = admission(&f);
    assert!(may_write(Rung::Value, &a) && may_write(Rung::RangeValue, &a));
    f.patterns = LiveRead::Value(vec![PatternKind::RangeValue]);
    let a = admission(&f);
    assert!(!may_write(Rung::Value, &a));
    assert!(may_write(Rung::RangeValue, &a));
    let a = Admission {
        route: AdmittedRoute::UiaWrite,
        value: PatternAdmission::Present {
            read_only: LiveRead::Value(false),
        },
        range: PatternAdmission::Absent,
    };
    assert!(!may_write(Rung::RangeValue, &a));
    assert!(!route_agrees(AdmittedRoute::UiaWrite, true));
    assert!(!route_agrees(AdmittedRoute::NativeCombo, false));
    let result = gate_then_write(
        &FakeClock::default(),
        Duration::ZERO,
        |_| std::future::ready(Probe::NativeCombo),
        |a| async move { a },
    )
    .await
    .unwrap();
    assert_eq!(result.route, AdmittedRoute::NativeCombo);
    let serialized = serde_json::to_value(admission_refusal("changed")).unwrap();
    assert_eq!(serialized["structuredContent"]["effect"], "refused");
    assert!(serialized["structuredContent"].get("disposition").is_none());
}
#[tokio::test]
async fn busy_workers_timeout_without_spawning() {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
    let _permit = semaphore.clone().try_acquire_owned().unwrap();
    let spawns = AtomicUsize::new(0);
    let writes = AtomicUsize::new(0);
    let clock = FakeClock::default();
    let result = gate_then_write(
        &clock,
        Duration::from_millis(200),
        |_| {
            limited_probe(semaphore.clone(), || {
                spawns.fetch_add(1, Ordering::SeqCst);
                || Probe::uia(raw())
            })
        },
        |_| async {
            writes.fetch_add(1, Ordering::SeqCst);
        },
    )
    .await;
    assert!(result.is_err());
    assert_eq!(spawns.load(Ordering::SeqCst), 0);
    assert_eq!(writes.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn permit_survives_cancelled_caller_and_worker_cleanup() {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
    let (uninit_tx, uninit_rx) = tokio::sync::oneshot::channel();
    let (release_uninit_tx, release_uninit_rx) = std::sync::mpsc::channel();
    struct Cleanup(
        std::sync::mpsc::Receiver<()>,
        Option<tokio::sync::oneshot::Sender<()>>,
    );
    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.1.take().unwrap().send(()).unwrap();
            self.0.recv().unwrap();
        }
    }
    let join = tokio::spawn(limited_probe(semaphore.clone(), move || {
        move || {
            let result = {
                let _cleanup = Cleanup(release_rx, Some(dropped_tx));
                entered_tx.send(()).unwrap();
                Probe::uia(raw())
            };
            uninit_tx.send(()).unwrap();
            release_uninit_rx.recv().unwrap();
            result
        }
    }));
    entered_rx.await.unwrap();
    dropped_rx.await.unwrap();
    assert_eq!(semaphore.available_permits(), 0);
    join.abort();
    assert_eq!(semaphore.available_permits(), 0);
    release_tx.send(()).unwrap();
    uninit_rx.await.unwrap();
    assert_eq!(semaphore.available_permits(), 0);
    release_uninit_tx.send(()).unwrap();
    let permit = semaphore.clone().acquire_owned().await.unwrap();
    assert_eq!(semaphore.available_permits(), 0);
    drop(permit);
    assert_eq!(semaphore.available_permits(), 1);
}
#[test]
fn value_notsupported_only_calls_admitted_range() {
    use crate::uia_mutation::{set_value_route, RungOutcome, SetValueDecision};
    for range_allowed in [false, true] {
        let admission = Admission {
            route: AdmittedRoute::UiaWrite,
            value: PatternAdmission::Present {
                read_only: LiveRead::Value(false),
            },
            range: if range_allowed {
                PatternAdmission::Present {
                    read_only: LiveRead::Value(false),
                }
            } else {
                PatternAdmission::Absent
            },
        };
        let calls = AtomicUsize::new(0);
        let mut refused = false;
        let decision = set_value_route(RungOutcome::Called(0x80040204u32 as i32), || {
            if !may_write(Rung::RangeValue, &admission) {
                refused = true;
                return RungOutcome::Absent;
            }
            calls.fetch_add(1, Ordering::SeqCst);
            RungOutcome::Called(0)
        });
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(!refused);
        assert!(matches!(
            decision,
            SetValueDecision::Uncertain { rung: 0, .. }
        ));
    }
}
#[test]
fn all_uia_patterns_map_to_expected_affordances() {
    let mut f = raw();
    f.patterns = LiveRead::Value(vec![
        PatternKind::Invoke,
        PatternKind::Toggle,
        PatternKind::SelectionItem,
        PatternKind::ExpandCollapse,
        PatternKind::Value,
        PatternKind::RangeValue,
        PatternKind::Scroll,
        PatternKind::ScrollItem,
    ]);
    assert_eq!(
        element_facts(&f, ValueWrite::Value).affordances,
        Evidence::Known(vec![
            Affordance::Press,
            Affordance::Toggle,
            Affordance::Select,
            Affordance::Expand,
            Affordance::Collapse,
            Affordance::SetValue,
            Affordance::Scroll,
            Affordance::ScrollIntoView
        ])
    );
}

#[test]
fn changed_patterns_never_call_an_unadmitted_rung() {
    use crate::uia_mutation::{set_value_route, RungOutcome, SetValueDecision};

    for (admission, worker_value, expected_value, expected_range) in [
        (
            Admission {
                route: AdmittedRoute::UiaWrite,
                value: PatternAdmission::Present {
                    read_only: LiveRead::Value(false),
                },
                range: PatternAdmission::Absent,
            },
            false,
            0,
            0,
        ),
        (
            Admission {
                route: AdmittedRoute::UiaWrite,
                value: PatternAdmission::Absent,
                range: PatternAdmission::Present {
                    read_only: LiveRead::Value(false),
                },
            },
            true,
            0,
            0,
        ),
        (
            Admission {
                route: AdmittedRoute::UiaWrite,
                value: PatternAdmission::Present {
                    read_only: LiveRead::Value(false),
                },
                range: PatternAdmission::Present {
                    read_only: LiveRead::Value(false),
                },
            },
            true,
            1,
            0,
        ),
    ] {
        let values = AtomicUsize::new(0);
        let ranges = AtomicUsize::new(0);
        if worker_value && !may_write(Rung::Value, &admission) {
            assert_eq!(values.load(Ordering::SeqCst), expected_value);
            assert_eq!(ranges.load(Ordering::SeqCst), expected_range);
            continue;
        }
        let value = if worker_value {
            values.fetch_add(1, Ordering::SeqCst);
            RungOutcome::Called(0x80040204u32 as i32)
        } else {
            RungOutcome::Absent
        };
        let result = set_value_route(value, || {
            if !may_write(Rung::RangeValue, &admission) {
                return RungOutcome::Absent;
            }
            ranges.fetch_add(1, Ordering::SeqCst);
            RungOutcome::Called(0)
        });
        assert_eq!(values.load(Ordering::SeqCst), expected_value);
        assert_eq!(ranges.load(Ordering::SeqCst), expected_range);
        if expected_range == 1 {
            assert_eq!(result, SetValueDecision::Delivered { rung: 1 });
        }
    }
}

#[test]
fn writable_range_admits_when_value_read_only_is_unknown() {
    let mut f = raw();
    f.patterns = LiveRead::Value(vec![PatternKind::Value, PatternKind::RangeValue]);
    f.value_read_only = LiveRead::Failed("value read failed".into(), None);
    f.range_read_only = LiveRead::Value(false);
    assert_eq!(evaluated(&f, ValueWrite::Value).verdict, Verdict::Pass);
    let a = admission(&f);
    assert!(!may_write(Rung::Value, &a));
    assert!(may_write(Rung::RangeValue, &a));
}

#[test]
fn admission_controls_value_and_range_calls() {
    use crate::uia_mutation::{set_value_route, RungOutcome, SetValueDecision};
    for (value, range, acquired, expected_value, expected_range, changed) in [
        (
            PatternAdmission::Present {
                read_only: LiveRead::Failed("unknown".into(), Some(-1)),
            },
            PatternAdmission::Present {
                read_only: LiveRead::Value(false),
            },
            true,
            0,
            1,
            false,
        ),
        (
            PatternAdmission::Present {
                read_only: LiveRead::Value(true),
            },
            PatternAdmission::Present {
                read_only: LiveRead::Value(false),
            },
            true,
            0,
            1,
            false,
        ),
        (
            PatternAdmission::Absent,
            PatternAdmission::Present {
                read_only: LiveRead::Value(false),
            },
            true,
            0,
            0,
            true,
        ),
        (
            PatternAdmission::Present {
                read_only: LiveRead::Value(false),
            },
            PatternAdmission::Present {
                read_only: LiveRead::Value(false),
            },
            true,
            1,
            0,
            false,
        ),
    ] {
        let admission = Admission {
            route: AdmittedRoute::UiaWrite,
            value,
            range,
        };
        let value_calls = AtomicUsize::new(0);
        let range_calls = AtomicUsize::new(0);
        let value = match value_pattern_route(&admission, acquired) {
            ValuePatternRoute::Changed => {
                assert!(changed);
                assert_eq!(value_calls.load(Ordering::SeqCst), expected_value);
                assert_eq!(range_calls.load(Ordering::SeqCst), expected_range);
                continue;
            }
            ValuePatternRoute::Absent | ValuePatternRoute::Skip => RungOutcome::Absent,
            ValuePatternRoute::Call => {
                value_calls.fetch_add(1, Ordering::SeqCst);
                RungOutcome::Called(1)
            }
        };
        let result = set_value_route(value, || {
            if may_write(Rung::RangeValue, &admission) {
                range_calls.fetch_add(1, Ordering::SeqCst);
                RungOutcome::Called(0)
            } else {
                RungOutcome::Absent
            }
        });
        assert_eq!(value_calls.load(Ordering::SeqCst), expected_value);
        assert_eq!(range_calls.load(Ordering::SeqCst), expected_range);
        if expected_value == 1 {
            assert!(matches!(
                result,
                SetValueDecision::Uncertain { rung: 0, .. }
            ));
        } else {
            assert_eq!(result, SetValueDecision::Delivered { rung: 1 });
        }
    }
}

#[test]
fn failed_live_property_names_stale_or_unconfirmed() {
    assert!(
        matches!(evidence(&LiveRead::<bool>::Failed("read".into(), Some(0x80040201u32 as i32))), Evidence::Unknown(message) if message.contains("stale element"))
    );
    assert!(
        matches!(evidence(&LiveRead::<bool>::Failed("read".into(), Some(0x80004005u32 as i32))), Evidence::Unknown(message) if message.contains("unconfirmed"))
    );
}
