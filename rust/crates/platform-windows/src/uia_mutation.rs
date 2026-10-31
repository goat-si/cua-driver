#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefusalKind {
    AccessDenied,
    ElementUnavailable,
    InvalidArgument,
    NotEnabled,
    /// Reading the pattern itself failed with something other than "not
    /// supported"; no method of that pattern was called.
    PatternUnreadable,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UncertainKind {
    Transport,
    Timeout,
    Unclassified,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteVerdict {
    Delivered,
    AffordanceAbsent,
    Refused(RefusalKind),
    Uncertain(UncertainKind),
}

pub(crate) fn classify_write_hresult(hresult: i32) -> WriteVerdict {
    use RefusalKind::*;
    use UncertainKind::*;
    use WriteVerdict::*;
    match hresult as u32 {
        0 => Delivered,
        0x8004_0204 => AffordanceAbsent,
        0x8007_0005 => Refused(AccessDenied),
        0x8004_0201 => Refused(ElementUnavailable),
        0x8007_0057 => Refused(InvalidArgument),
        0x8004_0200 => Refused(NotEnabled),
        0x8001_0105 | 0x8001_0108 | 0x8007_06ba | 0x8007_06be => Uncertain(Transport),
        0x8013_1505 => Uncertain(Timeout),
        _ => Uncertain(Unclassified),
    }
}
pub(crate) fn may_fall_through(verdict: &WriteVerdict) -> bool {
    matches!(verdict, WriteVerdict::AffordanceAbsent)
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
        not_supported_calls: Vec<usize>,
    },
    Refused {
        rung: usize,
        kind: RefusalKind,
        hresult: i32,
        not_supported_calls: Vec<usize>,
    },
    Uncertain {
        rung: usize,
        kind: UncertainKind,
        hresult: i32,
        not_supported_calls: Vec<usize>,
    },
    NoAffordance {
        not_supported_calls: Vec<usize>,
    },
}
pub(crate) fn run_rungs(rungs: Vec<Box<dyn FnOnce() -> RungOutcome + '_>>) -> ChainOutcome {
    let mut not_supported_calls = Vec::new();
    for (rung, call) in rungs.into_iter().enumerate() {
        let hresult = match call() {
            RungOutcome::Absent => continue,
            RungOutcome::AcquireFailed(hresult) => {
                return ChainOutcome::Refused {
                    rung,
                    kind: RefusalKind::PatternUnreadable,
                    hresult,
                    not_supported_calls,
                }
            }
            RungOutcome::Called(hresult) => hresult,
        };
        match classify_write_hresult(hresult) {
            WriteVerdict::Delivered => {
                return ChainOutcome::Delivered {
                    rung,
                    not_supported_calls,
                }
            }
            WriteVerdict::AffordanceAbsent => not_supported_calls.push(rung),
            WriteVerdict::Refused(kind) => {
                return ChainOutcome::Refused {
                    rung,
                    kind,
                    hresult,
                    not_supported_calls,
                }
            }
            WriteVerdict::Uncertain(kind) => {
                return ChainOutcome::Uncertain {
                    rung,
                    kind,
                    hresult,
                    not_supported_calls,
                }
            }
        }
    }
    ChainOutcome::NoAffordance {
        not_supported_calls,
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
        ChainOutcome::Delivered {
            rung,
            not_supported_calls,
        } => ClickPlan::Semantic {
            rung,
            failed: not_supported_calls,
        },
        ChainOutcome::NoAffordance {
            not_supported_calls,
        } => ClickPlan::Physical {
            failed: not_supported_calls,
        },
        ChainOutcome::Refused {
            rung,
            kind,
            hresult,
            not_supported_calls,
        } => ClickPlan::Refused {
            rung,
            kind,
            hresult,
            failed: not_supported_calls,
        },
        ChainOutcome::Uncertain {
            rung,
            kind,
            hresult,
            not_supported_calls,
        } => ClickPlan::Uncertain {
            rung,
            kind,
            hresult,
            failed: not_supported_calls,
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
        ChainOutcome::NoAffordance { .. } => SetValueDecision::NoAffordance,
    }
}
#[cfg(target_os = "windows")]
pub(crate) fn refusal_text(kind: RefusalKind, hresult: i32) -> String {
    let what = match kind {
        RefusalKind::PatternUnreadable => "could not read the UIA pattern",
        _ => "UIA refused the write",
    };
    format!(
        "{what}: {kind:?} (HRESULT 0x{:08X}); nothing was sent. Read the window state before retrying.",
        hresult as u32
    )
}
#[cfg(target_os = "windows")]
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
        Some(MutationStop::Write(hr))
            if matches!(classify_write_hresult(hr), WriteVerdict::Uncertain(_)) =>
        {
            Uncertain
        }
        Some(_) if completed > 0 => Partial,
        Some(MutationStop::Absent) => FallThrough,
        Some(MutationStop::Write(hr))
            if matches!(classify_write_hresult(hr), WriteVerdict::AffordanceAbsent) =>
        {
            FallThrough
        }
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
        RungOutcome::AcquireFailed(_) => MutationRoute::Refused,
        RungOutcome::Called(hr) => match classify_write_hresult(hr) {
            WriteVerdict::Delivered => MutationRoute::Done,
            WriteVerdict::AffordanceAbsent => MutationRoute::FallThrough,
            WriteVerdict::Refused(_) => MutationRoute::Refused,
            WriteVerdict::Uncertain(_) => MutationRoute::Uncertain,
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccelOutcome {
    Delivered,
    Absent,
    Stop(WriteVerdict, i32),
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
