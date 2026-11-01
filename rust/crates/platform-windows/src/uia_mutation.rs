#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefusalKind {
    StaleElement,
    PermissionDenied,
    UnconfirmedAlive,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UncertainKind {
    Provider(ProviderCode),
    Transport,
    Timeout,
    Unclassified,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderCode {
    NotSupported,
    AccessDenied,
    ElementUnavailable,
    InvalidArgument,
    NotEnabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SentVerdict {
    Delivered,
    Uncertain(UncertainKind),
}

pub(crate) fn classify_sent_call(hresult: i32) -> SentVerdict {
    use ProviderCode::*;
    use UncertainKind::*;
    let kind = match hresult as u32 {
        0 => return SentVerdict::Delivered,
        0x8004_0204 => Provider(NotSupported),
        0x8007_0005 => Provider(AccessDenied),
        0x8004_0201 => Provider(ElementUnavailable),
        0x8007_0057 => Provider(InvalidArgument),
        0x8004_0200 => Provider(NotEnabled),
        0x8001_0105 | 0x8001_0108 | 0x8007_06ba | 0x8007_06be => Transport,
        0x8013_1505 => Timeout,
        _ => Unclassified,
    };
    SentVerdict::Uncertain(kind)
}

/// Run a post-call step only when the provider returned exactly S_OK.
pub(crate) fn after_sent_call<T>(hresult: i32, next: impl FnOnce() -> T) -> Result<T, i32> {
    match classify_sent_call(hresult) {
        SentVerdict::Delivered => Ok(next()),
        SentVerdict::Uncertain(_) => Err(hresult),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FocusRoute {
    Confirmed,
    ClickBeforeSend,
    Uncertain,
}

pub(crate) fn focus_route(hresult: Option<i32>, confirm: impl FnOnce() -> bool) -> FocusRoute {
    match hresult {
        None => FocusRoute::ClickBeforeSend,
        Some(hr) => match after_sent_call(hr, confirm) {
            Ok(true) => FocusRoute::Confirmed,
            Ok(false) | Err(_) => FocusRoute::Uncertain,
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AcquireVerdict {
    Absent,
    NotDelivered(RefusalKind),
}

pub(crate) fn classify_acquisition(code: i32) -> AcquireVerdict {
    if pattern_absent(code) {
        return AcquireVerdict::Absent;
    }
    AcquireVerdict::NotDelivered(match code as u32 {
        0x8004_0201 => RefusalKind::StaleElement,
        0x8007_0005 => RefusalKind::PermissionDenied,
        _ => RefusalKind::UnconfirmedAlive,
    })
}

impl SentVerdict {
    pub(crate) fn to_core(self) -> cua_driver_core::mutation::WriteOutcome {
        use cua_driver_core::mutation::{Reason, WriteOutcome};
        match self {
            Self::Delivered => WriteOutcome::Delivered,
            Self::Uncertain(kind) => WriteOutcome::StopUncertain(match kind {
                UncertainKind::Transport => Reason::Transport,
                UncertainKind::Timeout => Reason::Timeout,
                UncertainKind::Provider(_) | UncertainKind::Unclassified => Reason::Unknown,
            }),
        }
    }
}
impl AcquireVerdict {
    pub(crate) fn to_core(self) -> cua_driver_core::mutation::WriteOutcome {
        use cua_driver_core::mutation::{Reason, WriteOutcome};
        match self {
            Self::Absent => WriteOutcome::FallThrough(Reason::Unsupported),
            Self::NotDelivered(kind) => WriteOutcome::StopNotDelivered(match kind {
                RefusalKind::StaleElement => Reason::StaleElement,
                RefusalKind::PermissionDenied => Reason::PermissionDenied,
                RefusalKind::UnconfirmedAlive => Reason::UnconfirmedAlive,
            }),
        }
    }
}
/// Whether a failed `GetCurrentPattern` or interface cast means the pattern
/// is simply not there. windows-rs reports a successful null pattern as an
/// error with a success code, so any non-negative code counts as absent, as do
/// `E_NOINTERFACE` and `UIA_E_NOTSUPPORTED`. Anything else is a failed read.
pub(crate) fn pattern_absent(code: i32) -> bool {
    code >= 0 || code as u32 == 0x8000_4002 || code as u32 == 0x8004_0204
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RungOutcome {
    Absent,
    /// The pattern could not be read (see [`pattern_absent`]); nothing was sent.
    AcquireFailed(i32),
    Called(i32),
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ChainOutcome {
    Delivered {
        rung: usize,
    },
    Refused {
        rung: usize,
        kind: RefusalKind,
        hresult: i32,
    },
    Uncertain {
        rung: usize,
        kind: UncertainKind,
        hresult: i32,
    },
    NoAffordance,
}
pub(crate) fn run_rungs(rungs: Vec<Box<dyn FnOnce() -> RungOutcome + '_>>) -> ChainOutcome {
    use cua_driver_core::mutation::{WriteChain, WriteOutcome};
    let mut last = None;
    let chain = WriteChain::run(
        rungs.into_iter().enumerate().map(|(index, call)| {
            let outcome = call();
            last = Some((index, outcome));
            match outcome {
                RungOutcome::Absent => AcquireVerdict::Absent.to_core(),
                RungOutcome::AcquireFailed(hr) => classify_acquisition(hr).to_core(),
                RungOutcome::Called(hr) => classify_sent_call(hr).to_core(),
            }
        }),
        false,
    );
    match (chain.outcomes().last(), last) {
        (Some(WriteOutcome::Delivered), Some((rung, _))) => ChainOutcome::Delivered { rung },
        (
            Some(WriteOutcome::StopNotDelivered(_)),
            Some((rung, RungOutcome::AcquireFailed(hresult))),
        ) => {
            let AcquireVerdict::NotDelivered(kind) = classify_acquisition(hresult) else {
                unreachable!()
            };
            ChainOutcome::Refused {
                rung,
                kind,
                hresult,
            }
        }
        (Some(WriteOutcome::StopUncertain(_)), Some((rung, RungOutcome::Called(hresult)))) => {
            let SentVerdict::Uncertain(kind) = classify_sent_call(hresult) else {
                unreachable!()
            };
            ChainOutcome::Uncertain {
                rung,
                kind,
                hresult,
            }
        }
        (None | Some(WriteOutcome::FallThrough(_)), _) => ChainOutcome::NoAffordance,
        _ => unreachable!("rung outcome and core result disagree"),
    }
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ClickPlan {
    Semantic {
        rung: usize,
        failed: Vec<usize>,
    },
    Physical {
        failed: Vec<usize>,
    },
    Refused {
        rung: usize,
        kind: RefusalKind,
        hresult: i32,
        failed: Vec<usize>,
    },
    Uncertain {
        rung: usize,
        kind: UncertainKind,
        hresult: i32,
        failed: Vec<usize>,
    },
}
pub(crate) fn click_plan_from_chain(outcome: ChainOutcome) -> ClickPlan {
    match outcome {
        ChainOutcome::Delivered { rung } => ClickPlan::Semantic {
            rung,
            failed: vec![],
        },
        ChainOutcome::NoAffordance => ClickPlan::Physical { failed: vec![] },
        ChainOutcome::Refused {
            rung,
            kind,
            hresult,
        } => ClickPlan::Refused {
            rung,
            kind,
            hresult,
            failed: vec![],
        },
        ChainOutcome::Uncertain {
            rung,
            kind,
            hresult,
        } => ClickPlan::Uncertain {
            rung,
            kind,
            hresult,
            failed: vec![],
        },
    }
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SetValueDecision {
    Delivered {
        rung: usize,
    },
    Refused {
        rung: usize,
        kind: RefusalKind,
        hresult: i32,
    },
    Uncertain {
        rung: usize,
        kind: UncertainKind,
        hresult: i32,
    },
    NoAffordance,
}
pub(crate) fn set_value_route(
    value_rung: RungOutcome,
    range_rung: impl FnOnce() -> RungOutcome,
) -> SetValueDecision {
    match run_rungs(vec![Box::new(|| value_rung), Box::new(range_rung)]) {
        ChainOutcome::Delivered { rung, .. } => SetValueDecision::Delivered { rung },
        ChainOutcome::Refused {
            rung,
            kind,
            hresult,
            ..
        } => SetValueDecision::Refused {
            rung,
            kind,
            hresult,
        },
        ChainOutcome::Uncertain {
            rung,
            kind,
            hresult,
            ..
        } => SetValueDecision::Uncertain {
            rung,
            kind,
            hresult,
        },
        ChainOutcome::NoAffordance => SetValueDecision::NoAffordance,
    }
}
pub(crate) fn refusal_text(kind: RefusalKind, hresult: i32) -> String {
    let what = "could not read the UIA pattern";
    format!(
        "{what}: {kind:?} (HRESULT 0x{:08X}); nothing was sent. Read the window state before retrying.",
        hresult as u32
    )
}
pub(crate) fn uncertain_text(kind: UncertainKind, hresult: i32) -> String {
    format!("HRESULT 0x{:08X} ({kind:?}); the UIA write may have landed. Read the window state before acting again.", hresult as u32)
}
#[cfg(test)]
#[path = "uia_mutation_tests.rs"]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MutationStop {
    Absent,
    AcquireFailed(i32),
    Write(i32),
    WorkerFailed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MutationRoute {
    Done,
    FallThrough,
    Refused,
    Partial,
    Uncertain,
}
pub(crate) fn scroll_route(completed: u32, stop: Option<MutationStop>) -> MutationRoute {
    mutation_route(completed as usize, stop)
}
fn mutation_route(completed: usize, stop: Option<MutationStop>) -> MutationRoute {
    use MutationRoute::*;
    match stop {
        None => Done,
        Some(MutationStop::WorkerFailed) => Uncertain,
        Some(MutationStop::Write(_)) => Uncertain,
        Some(_) if completed > 0 => Partial,
        Some(MutationStop::Absent) => FallThrough,
        Some(_) => Refused,
    }
}
pub(crate) fn multi_invoke_route(delivered: usize, stop: Option<MutationStop>) -> MutationRoute {
    mutation_route(delivered, stop)
}
pub(crate) fn scroll_direction(direction: &str) -> Option<(bool, bool)> {
    match direction {
        "up" => Some((false, false)),
        "down" => Some((false, true)),
        "left" => Some((true, false)),
        "right" => Some((true, true)),
        _ => None,
    }
}
/// Validate the direction before acquiring a COM interface or calling a provider.
pub(crate) fn scroll_preflight<T>(
    direction: &str,
    acquire: impl FnOnce() -> T,
) -> Option<(bool, bool, T)> {
    let (horizontal, increment) = scroll_direction(direction)?;
    Some((horizontal, increment, acquire()))
}

pub(crate) fn expand_route(outcome: RungOutcome) -> MutationRoute {
    match outcome {
        RungOutcome::Absent => MutationRoute::FallThrough,
        RungOutcome::AcquireFailed(hr) => match classify_acquisition(hr) {
            AcquireVerdict::Absent => MutationRoute::FallThrough,
            AcquireVerdict::NotDelivered(_) => MutationRoute::Refused,
        },
        RungOutcome::Called(hr) => match classify_sent_call(hr) {
            SentVerdict::Delivered => MutationRoute::Done,
            SentVerdict::Uncertain(_) => MutationRoute::Uncertain,
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccelStop {
    Refused(RefusalKind),
    Uncertain(UncertainKind),
    /// The scan never started (gate busy, or the worker could not be spawned).
    NotSent(&'static str),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccelOutcome {
    Delivered,
    Absent,
    Stop(AccelStop, i32),
}
pub(crate) fn accelerator_scan(outcomes: impl Iterator<Item = AccelOutcome>) -> AccelOutcome {
    for outcome in outcomes {
        if outcome != AccelOutcome::Absent {
            return outcome;
        }
    }
    AccelOutcome::Absent
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MenuFailure {
    Refused,
    Uncertain,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MenuResult {
    Done,
    Refused,
    Partial { completed_hops: usize },
    Uncertain { completed_hops: usize },
}
pub(crate) fn menu_outcome(completed_hops: usize, failure: Option<MenuFailure>) -> MenuResult {
    match failure {
        None => MenuResult::Done,
        Some(MenuFailure::Uncertain) => MenuResult::Uncertain { completed_hops },
        Some(MenuFailure::Refused) if completed_hops > 0 => MenuResult::Partial { completed_hops },
        Some(MenuFailure::Refused) => MenuResult::Refused,
    }
}

/// Preserve a producer error while accounting for a prior side effect.
pub(crate) fn after_prologue(
    mut result: cua_driver_core::protocol::ToolResult,
) -> cua_driver_core::protocol::ToolResult {
    use cua_driver_core::mutation::{Postcondition, Reason, WriteChain, WriteOutcome};
    let chain = WriteChain::run(
        [WriteOutcome::StopNotDelivered(Reason::UnconfirmedAlive)],
        true,
    );
    let structured = result
        .structured_content
        .get_or_insert_with(|| serde_json::json!({}));
    if !structured.is_object() {
        *structured = serde_json::json!({ "detail": structured.take() });
    }
    structured["effect"] = serde_json::to_value(chain.effect(Postcondition::Indeterminate, false))
        .expect("effect serializes");
    result.disposition = chain.error_disposition(Postcondition::Indeterminate, false);
    if let Some(record) = result.action_record.as_mut() {
        record.effect = cua_driver_core::action_record::ActionEffect::Unverifiable;
    }
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PointFailure {
    Busy,
    Unavailable,
    Timeout,
    SentFailure,
}

pub(crate) fn point_failure_result(
    failure: PointFailure,
    pid: u32,
) -> cua_driver_core::protocol::ToolResult {
    use cua_driver_core::mutation::{Reason, WriteOutcome};
    let (status, outcome, message) = match failure {
        PointFailure::Busy => (
            "busy",
            WriteOutcome::StopNotDelivered(Reason::UnconfirmedAlive),
            "nothing was sent",
        ),
        PointFailure::Unavailable => (
            "unavailable",
            WriteOutcome::StopNotDelivered(Reason::UnconfirmedAlive),
            "nothing was sent",
        ),
        PointFailure::Timeout => (
            "timeout",
            WriteOutcome::StopUncertain(Reason::Timeout),
            "the click may have landed; inspect the target state before acting again",
        ),
        PointFailure::SentFailure => (
            "sent_failure",
            WriteOutcome::StopUncertain(Reason::Unknown),
            "the click may have landed; inspect the target state before acting again",
        ),
    };
    let mut fields = serde_json::json!({"uia_status":status, "path":"ax", "verified":false, "code":"background_unavailable"});
    if matches!(failure, PointFailure::Busy | PointFailure::Unavailable) {
        fields["nothing_sent"] = true.into();
        fields["suggestion"] = "Retry this action with delivery_mode:\"foreground\".".into();
        fields["escalation"] = serde_json::json!({"recommended":"foreground", "reason":format!("the UIA provider is {status}; retry this action with delivery_mode:\"foreground\".")});
    }
    chain_error(
        format!("UIA pixel click {status} for pid {pid}; {message}. No fallback input was sent."),
        fields,
        [outcome],
        false,
    )
}

pub(crate) fn foreground_focus_uncertain_result(
    message: impl Into<String>,
) -> cua_driver_core::protocol::ToolResult {
    use cua_driver_core::mutation::{Reason, WriteOutcome};
    chain_error(
        message,
        serde_json::json!({"path":"uia", "verified":false}),
        [WriteOutcome::StopUncertain(Reason::Unknown)],
        true,
    )
}

/// Construct an error from the complete chain, retaining the producer's legacy fields.
pub(crate) fn chain_error(
    message: impl Into<String>,
    mut structured: serde_json::Value,
    outcomes: impl IntoIterator<Item = cua_driver_core::mutation::WriteOutcome>,
    side_effectful_prologue: bool,
) -> cua_driver_core::protocol::ToolResult {
    use cua_driver_core::mutation::{Postcondition, WriteChain};
    let chain = WriteChain::run(outcomes, side_effectful_prologue);
    structured["effect"] = serde_json::to_value(chain.effect(Postcondition::Indeterminate, false))
        .expect("action effect serializes");
    let disposition = chain
        .error_disposition(Postcondition::Indeterminate, false)
        .expect("an indeterminate error has a disposition");
    cua_driver_core::protocol::ToolResult::error(message)
        .with_structured(structured)
        .with_disposition(disposition)
}

pub(crate) fn set_value_write_failure_result(
    decision: SetValueDecision,
    args: &serde_json::Value,
) -> cua_driver_core::protocol::ToolResult {
    use cua_driver_core::action_record::{
        ActionEffect, ActionExecutionRecord, ActionTransport, RequestedDelivery,
    };
    let (rung, message, outcome) = match decision {
        SetValueDecision::Refused {
            rung,
            kind,
            hresult,
        } => (
            rung,
            refusal_text(kind, hresult),
            AcquireVerdict::NotDelivered(kind).to_core(),
        ),
        SetValueDecision::Uncertain {
            rung,
            kind,
            hresult,
        } => (
            rung,
            uncertain_text(kind, hresult),
            classify_sent_call(hresult).to_core(),
        ),
        _ => unreachable!("only write failures are returned here"),
    };
    let transport = if rung == 0 {
        ActionTransport::WindowsUiaValue
    } else {
        ActionTransport::WindowsUiaRangeValue
    };
    let requested = match args
        .get("delivery_mode")
        .and_then(serde_json::Value::as_str)
    {
        Some("foreground") => RequestedDelivery::Foreground,
        _ => RequestedDelivery::Background,
    };
    let mut record = ActionExecutionRecord::new(ActionEffect::Unverifiable, transport, requested);
    if matches!(
        outcome,
        cua_driver_core::mutation::WriteOutcome::StopUncertain(_)
    ) {
        record.actual_delivery = Some(cua_driver_core::action_record::ActualDelivery::Unknown);
    }
    chain_error(
        message,
        serde_json::json!({ "path": "ax", "verified": false }),
        [outcome],
        true,
    )
    .with_action_record(record)
}
