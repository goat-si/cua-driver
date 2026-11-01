use cua_driver_core::actionability::{
    auto_wait, evaluate, ActionabilityReport, Affordance, Clock, ElementFacts, Evidence, Intent,
    Policy, Rect, Verdict,
};
use cua_driver_core::protocol::ToolResult;
use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LiveRead<T> {
    Value(T),
    NotSupported,
    Failed(String, Option<i32>),
}

pub(crate) fn evidence<T: Clone>(read: &LiveRead<T>) -> Evidence<T> {
    match read {
        LiveRead::Value(v) => Evidence::Known(v.clone()),
        LiveRead::NotSupported => Evidence::Absent,
        LiveRead::Failed(e, _) => Evidence::Unknown(e.clone()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PatternKind {
    Invoke,
    Toggle,
    SelectionItem,
    ExpandCollapse,
    Value,
    RangeValue,
    Scroll,
    ScrollItem,
}
#[derive(Debug, Clone)]
pub(crate) struct RawLiveFacts {
    pub is_offscreen: LiveRead<bool>,
    pub is_enabled: LiveRead<bool>,
    pub bounds: LiveRead<Rect>,
    pub window_visible: LiveRead<bool>,
    pub window_minimized: LiveRead<bool>,
    pub window_cloaked: LiveRead<bool>,
    pub control_type: LiveRead<String>,
    pub patterns: LiveRead<Vec<PatternKind>>,
    pub value_read_only: LiveRead<bool>,
    pub range_read_only: LiveRead<bool>,
}
impl RawLiveFacts {
    pub(crate) fn failed(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            is_offscreen: LiveRead::Failed(reason.clone(), None),
            is_enabled: LiveRead::Failed(reason.clone(), None),
            bounds: LiveRead::Failed(reason.clone(), None),
            window_visible: LiveRead::Failed(reason.clone(), None),
            window_minimized: LiveRead::Failed(reason.clone(), None),
            window_cloaked: LiveRead::Failed(reason.clone(), None),
            control_type: LiveRead::Failed(reason.clone(), None),
            patterns: LiveRead::Failed(reason.clone(), None),
            value_read_only: LiveRead::Failed(reason.clone(), None),
            range_read_only: LiveRead::Failed(reason, None),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValueWrite {
    Value,
    RangeValue,
}

pub(crate) fn element_facts(raw: &RawLiveFacts, _write: ValueWrite) -> ElementFacts {
    let checks = [
        (&raw.is_offscreen, false),
        (&raw.window_visible, true),
        (&raw.window_minimized, false),
        (&raw.window_cloaked, false),
    ];
    let visible = if checks
        .iter()
        .any(|(r, expected)| matches!(r, LiveRead::Value(v) if v != expected))
    {
        Evidence::Known(false)
    } else if checks
        .iter()
        .all(|(r, expected)| matches!(r, LiveRead::Value(v) if v == expected))
    {
        Evidence::Known(true)
    } else {
        Evidence::Unknown(
            checks
                .iter()
                .find_map(|(r, _)| match r {
                    LiveRead::Failed(e, _) => Some(e.clone()),
                    LiveRead::NotSupported => Some("visibility unavailable".into()),
                    _ => None,
                })
                .unwrap_or_else(|| "visibility unavailable".into()),
        )
    };
    let editable = match &raw.patterns {
        LiveRead::Value(patterns) => {
            let reads = [
                (PatternKind::Value, &raw.value_read_only),
                (PatternKind::RangeValue, &raw.range_read_only),
            ];
            let present: Vec<_> = reads
                .iter()
                .filter(|(kind, _)| patterns.contains(kind))
                .collect();
            if present.is_empty() {
                Evidence::Absent
            } else if present
                .iter()
                .any(|(_, read)| matches!(read, LiveRead::Value(false)))
            {
                Evidence::Known(true)
            } else if present
                .iter()
                .all(|(_, read)| matches!(read, LiveRead::Value(true)))
            {
                Evidence::Known(false)
            } else {
                Evidence::Unknown("read-only state unavailable".into())
            }
        }
        LiveRead::NotSupported => Evidence::Absent,
        LiveRead::Failed(e, _) => Evidence::Unknown(e.clone()),
    };
    let affordances = match &raw.patterns {
        LiveRead::Value(patterns) => {
            let mut actions = Vec::new();
            for p in patterns {
                let mapped: &[Affordance] = match p {
                    PatternKind::Invoke => &[Affordance::Press],
                    PatternKind::Toggle => &[Affordance::Toggle],
                    PatternKind::SelectionItem => &[Affordance::Select],
                    PatternKind::ExpandCollapse => &[Affordance::Expand, Affordance::Collapse],
                    PatternKind::Value | PatternKind::RangeValue => &[Affordance::SetValue],
                    PatternKind::Scroll => &[Affordance::Scroll],
                    PatternKind::ScrollItem => &[Affordance::ScrollIntoView],
                };
                for a in mapped {
                    if !actions.contains(a) {
                        actions.push(*a);
                    }
                }
            }
            Evidence::Known(actions)
        }
        LiveRead::NotSupported => Evidence::Absent,
        LiveRead::Failed(e, _) => Evidence::Unknown(e.clone()),
    };
    let role = match &raw.control_type {
        LiveRead::Value(name) => Evidence::Known(match name.as_str() {
            "Edit" => "textfield".into(),
            "ComboBox" => "combobox".into(),
            _ => name.to_lowercase(),
        }),
        LiveRead::NotSupported => Evidence::Absent,
        LiveRead::Failed(e, _) => Evidence::Unknown(e.clone()),
    };
    ElementFacts {
        visible,
        offscreen: evidence(&raw.is_offscreen),
        enabled: evidence(&raw.is_enabled),
        editable,
        stable: Evidence::Absent,
        receives_events: Evidence::Absent,
        bounds: evidence(&raw.bounds),
        affordances,
        advertised_actions: Evidence::Absent,
        selectable_text: Evidence::Absent,
        role,
        nonspatial_visible: false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdmittedRoute {
    UiaWrite,
    NativeCombo,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Admission {
    pub route: AdmittedRoute,
    pub value: PatternAdmission,
    pub range: PatternAdmission,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PatternAdmission {
    Absent,
    Present { read_only: LiveRead<bool> },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rung {
    Value,
    RangeValue,
}
pub(crate) fn may_write(rung: Rung, a: &Admission) -> bool {
    a.route == AdmittedRoute::UiaWrite
        && match rung {
            Rung::Value => matches!(
                &a.value,
                PatternAdmission::Present {
                    read_only: LiveRead::Value(false)
                }
            ),
            Rung::RangeValue => matches!(
                &a.range,
                PatternAdmission::Present {
                    read_only: LiveRead::Value(false)
                }
            ),
        }
}
pub(crate) fn route_agrees(route: AdmittedRoute, worker_native: bool) -> bool {
    (route == AdmittedRoute::NativeCombo) == worker_native
}
pub(crate) enum Probe {
    NativeCombo,
    Uia(Box<RawLiveFacts>),
}
impl Probe {
    pub(crate) fn uia(facts: RawLiveFacts) -> Self {
        Self::Uia(Box::new(facts))
    }
}
fn admission(raw: &RawLiveFacts) -> Admission {
    let rung = |kind, read: &LiveRead<bool>| {
        if matches!(&raw.patterns, LiveRead::Value(patterns) if patterns.contains(&kind)) {
            PatternAdmission::Present { read_only: read.clone() }
        } else {
            PatternAdmission::Absent
        }
    };
    Admission {
        route: AdmittedRoute::UiaWrite,
        value: rung(PatternKind::Value, &raw.value_read_only),
        range: rung(PatternKind::RangeValue, &raw.range_read_only),
    }
}

pub(crate) type GateOutcome<T> = Result<T, ToolResult>;

pub(crate) async fn gate_then_write<C, P, F, W, Fut, T>(
    clock: &C,
    budget: Duration,
    mut probe: P,
    write: W,
) -> GateOutcome<T>
where
    C: Clock + Sync,
    P: FnMut(Duration) -> F + Send,
    F: Future<Output = Probe> + Send,
    W: FnOnce(Admission) -> Fut,
    Fut: Future<Output = T>,
{
    let admitted = Arc::new(Mutex::new(None));
    let report = auto_wait(clock, budget, false, |remaining| {
        let probe = probe(remaining);
        let admitted = admitted.clone();
        async move {
            let (facts, admission) = match probe.await {
                Probe::NativeCombo => {
                    // PR 1 does not gate native Win32 combo selection (PR 3 gates it
                    // as Select). Pass without facts rather than inventing evidence.
                    *admitted.lock().expect("admission lock") = Some(Admission {
                        route: AdmittedRoute::NativeCombo,
                        value: PatternAdmission::Absent,
                        range: PatternAdmission::Absent,
                    });
                    return ActionabilityReport {
                        verdict: Verdict::Pass,
                        checks: Vec::new(),
                        waited_ms: 0,
                        samples: 1,
                        last_facts: None,
                        disposition: None,
                    };
                }
                Probe::Uia(raw) => {
                    let a = admission(&raw);
                    let value_present = matches!(
                        &raw.patterns,
                        LiveRead::Value(v) if v.contains(&PatternKind::Value)
                    );
                    let first = if value_present {
                        ValueWrite::Value
                    } else {
                        ValueWrite::RangeValue
                    };
                    (element_facts(&raw, first), a)
                }
            };
            let report = evaluate(Intent::SetValue, &facts, Policy::headless());
            if matches!(report.verdict, Verdict::Pass) {
                *admitted.lock().expect("admission lock") = Some(admission);
            }
            report
        }
    })
    .await;
    if matches!(report.verdict, Verdict::Pass) {
        let admission = admitted
            .lock()
            .expect("admission lock")
            .take()
            .expect("passing probe records admission");
        Ok(write(admission).await)
    } else {
        Err(report
            .refusal_result()
            .expect("auto_wait returns pass or terminal"))
    }
}

// The permit lives inside the blocking closure, even if its JoinHandle is dropped.
pub(crate) async fn limited_probe<M, F>(
    semaphore: Arc<tokio::sync::Semaphore>,
    make_worker: M,
) -> Probe
where
    M: FnOnce() -> F,
    F: FnOnce() -> Probe + Send + 'static,
{
    let Ok(permit) = semaphore.try_acquire_owned() else {
        return Probe::uia(RawLiveFacts::failed("live read workers busy"));
    };
    let worker = make_worker();
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        worker()
    })
    .await
    {
        Ok(probe) => probe,
        Err(error) => Probe::uia(RawLiveFacts::failed(format!(
            "live read worker failed: {error}"
        ))),
    }
}

pub(crate) fn admission_refusal(reason: &str) -> ToolResult {
    ToolResult::error(reason).with_structured(serde_json::json!({"code":"element_not_actionable", "effect":"refused", "refusal":{"code":"element_not_actionable", "message":reason}}))
}
#[cfg(test)]
#[path = "actionability_tests.rs"]
mod tests;
