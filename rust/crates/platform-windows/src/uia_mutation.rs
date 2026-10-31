#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefusalKind {
    AccessDenied,
    ElementUnavailable,
    InvalidArgument,
    NotEnabled,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RungOutcome {
    Absent,
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
        let RungOutcome::Called(hresult) = call() else {
            continue;
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
    format!(
        "UIA write refused: {kind:?} (HRESULT 0x{:08X}); read the window state before retrying.",
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
