use super::{
    supported_affordances, ActionabilityReport, Affordance, Check, CheckStatus, ElementFacts,
    Evidence, Intent,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    allow_cursor_move: bool,
    allow_focus_steal: bool,
}
impl Policy {
    pub const fn headless() -> Self {
        Self {
            allow_cursor_move: false,
            allow_focus_steal: false,
        }
    }
    pub const fn focus_fallback() -> Self {
        Self {
            allow_cursor_move: false,
            allow_focus_steal: true,
        }
    }
    pub const fn headed() -> Self {
        Self {
            allow_cursor_move: true,
            allow_focus_steal: true,
        }
    }
    fn is_headed(self) -> bool {
        self.allow_cursor_move && self.allow_focus_steal
    }
}

fn push(
    checks: &mut Vec<Check>,
    name: &'static str,
    status: CheckStatus,
    reason: impl Into<String>,
) {
    checks.push(Check {
        name,
        status,
        reason: reason.into(),
        occluder: None,
    });
}
fn gate(checks: &mut Vec<Check>, name: &'static str, evidence: &Evidence<bool>, terminal: bool) {
    match evidence {
        Evidence::Known(true) => push(checks, name, CheckStatus::Pass, ""),
        Evidence::Known(false) | Evidence::Absent if terminal => push(
            checks,
            name,
            CheckStatus::Terminal {
                code: "unsupported_action",
            },
            format!("{name} is not present"),
        ),
        Evidence::Known(false) | Evidence::Absent => push(
            checks,
            name,
            CheckStatus::Transient { unknown: false },
            format!("{name} is false or absent"),
        ),
        Evidence::Unknown(reason) => push(
            checks,
            name,
            CheckStatus::Transient { unknown: true },
            format!("{name}: {reason}"),
        ),
    }
}

/// Evaluates only live evidence supplied by the caller; no platform calls occur.
pub fn evaluate(intent: Intent, facts: &ElementFacts, policy: Policy) -> ActionabilityReport {
    use Intent::*;
    let requirements = intent.requirements();
    let mut checks = Vec::new();
    let actions = match &facts.affordances {
        Evidence::Known(v) => Some(v.as_slice()),
        Evidence::Absent => Some(&[][..]),
        Evidence::Unknown(_) => None,
    };
    let has = |wanted: &[Affordance]| actions.is_some_and(|a| wanted.iter().any(|v| a.contains(v)));
    let direct = match intent {
        Click => has(&[Affordance::Press]),
        RightClick => has(&[Affordance::ShowMenu]),
        _ => false,
    };
    let semantic_pointer = requirements.receives_events && !policy.is_headed() && direct;
    let without_pointer = policy == Policy::headless()
        && (semantic_pointer
            || (!requirements.receives_events && has(supported_affordances(intent))));
    let keyboard = matches!(intent, TypeText | PressKey | KeyDown | KeyUp);
    let geometry_needed =
        !keyboard && !(without_pointer && (semantic_pointer || requirements.editable));
    if requirements.visible {
        gate(&mut checks, "visible", &facts.visible, false);
        match &facts.offscreen {
            Evidence::Known(true) => push(
                &mut checks,
                "offscreen",
                CheckStatus::Transient { unknown: false },
                "target is offscreen",
            ),
            Evidence::Unknown(reason) => push(
                &mut checks,
                "offscreen",
                CheckStatus::Transient { unknown: true },
                reason,
            ),
            Evidence::Known(false) | Evidence::Absent => {}
        }
        match &facts.bounds {
            Evidence::Known(r) if !r.valid() => push(
                &mut checks,
                "geometry",
                CheckStatus::Transient { unknown: true },
                "bounds are invalid",
            ),
            Evidence::Known(r)
                if (r.width > 0.0 && r.height > 0.0)
                    || (!geometry_needed && matches!(facts.offscreen, Evidence::Absent)) => {}
            Evidence::Known(_) => push(
                &mut checks,
                "geometry",
                CheckStatus::Transient { unknown: false },
                "bounds are empty",
            ),
            Evidence::Absent
                if ((!geometry_needed || without_pointer)
                    && matches!(facts.offscreen, Evidence::Known(false)))
                    || (without_pointer && facts.nonspatial_visible) => {}
            Evidence::Absent => push(
                &mut checks,
                "geometry",
                CheckStatus::Transient { unknown: true },
                "bounds are absent",
            ),
            Evidence::Unknown(reason) => push(
                &mut checks,
                "geometry",
                CheckStatus::Transient { unknown: true },
                reason,
            ),
        }
    }
    if requirements.stable && !semantic_pointer {
        let unmeasurable_without_pointer = without_pointer
            && matches!(facts.bounds, Evidence::Absent)
            && matches!(facts.offscreen, Evidence::Known(false))
            && !matches!(facts.stable, Evidence::Unknown(_));
        if !unmeasurable_without_pointer {
            gate(&mut checks, "stable", &facts.stable, false);
        }
    }
    if requirements.enabled {
        gate(&mut checks, "enabled", &facts.enabled, false);
    }
    if requirements.editable {
        let role_editable = matches!(&facts.role, Evidence::Known(role) if role == "textfield" || role == "combobox");
        match &facts.editable {
            Evidence::Known(_) | Evidence::Unknown(_) => {
                gate(&mut checks, "editable", &facts.editable, true)
            }
            Evidence::Absent if role_editable || has(&[Affordance::SetValue]) => {
                push(&mut checks, "editable", CheckStatus::Pass, "")
            }
            Evidence::Absent if matches!(facts.role, Evidence::Unknown(_)) || actions.is_none() => {
                push(
                    &mut checks,
                    "editable",
                    CheckStatus::Transient { unknown: true },
                    "editability evidence unavailable",
                )
            }
            Evidence::Absent => gate(&mut checks, "editable", &facts.editable, true),
        }
    }
    let physical_allowed = policy.allow_cursor_move && policy.allow_focus_steal;
    let support = if matches!(intent, Hover | Drag | ScrollTo) {
        Some(true)
    } else if matches!(intent, Click | RightClick) {
        if (!policy.is_headed() && direct) || physical_allowed {
            Some(true)
        } else if actions.is_none() && !policy.is_headed() {
            None
        } else {
            Some(false)
        }
    } else if matches!(intent, DoubleClick | TripleClick) {
        Some(physical_allowed)
    } else if (intent == Select
        && matches!(&facts.role, Evidence::Known(role) if role == "combobox")
        && has(&[Affordance::Expand, Affordance::Collapse]))
        || (matches!(intent, TypeText | PressKey) && policy.allow_focus_steal)
    {
        Some(true)
    } else if intent == Select
        && matches!(facts.role, Evidence::Unknown(_))
        && has(&[Affordance::Expand, Affordance::Collapse])
        && !has(supported_affordances(intent))
    {
        None
    } else {
        actions.map(|_| has(supported_affordances(intent)))
    };
    match support {
        Some(true) => push(&mut checks, "supported_action", CheckStatus::Pass, ""),
        Some(false) => {
            let code = if matches!(
                intent,
                Click | RightClick | DoubleClick | TripleClick | TypeText | PressKey
            ) {
                "policy_denied"
            } else {
                "unsupported_action"
            };
            push(
                &mut checks,
                "supported_action",
                CheckStatus::Terminal { code },
                "delivery is unavailable under this policy",
            );
        }
        None => push(
            &mut checks,
            "supported_action",
            CheckStatus::Transient { unknown: true },
            "affordances unavailable",
        ),
    }
    if matches!(intent, Hover | Drag) && !policy.allow_cursor_move {
        push(
            &mut checks,
            "policy",
            CheckStatus::Terminal {
                code: "policy_denied",
            },
            "cursor movement denied",
        );
    }
    if requirements.receives_events && !semantic_pointer {
        match &facts.receives_events {
            Evidence::Known(e) if e.verified_point.is_some() => {
                push(&mut checks, "receives_events", CheckStatus::Pass, "")
            }
            Evidence::Known(e) if e.unknown > 0 => push(
                &mut checks,
                "receives_events",
                CheckStatus::Transient { unknown: true },
                "hit test inconclusive",
            ),
            Evidence::Known(_) | Evidence::Absent => push(
                &mut checks,
                "receives_events",
                CheckStatus::Transient { unknown: false },
                "target occluded or hit test absent",
            ),
            Evidence::Unknown(reason) => push(
                &mut checks,
                "receives_events",
                CheckStatus::Transient { unknown: true },
                reason,
            ),
        }
        if let Evidence::Known(hit) = &facts.receives_events {
            if let Some(check) = checks.last_mut() {
                check.occluder = hit.occluder.clone();
            }
        }
    }
    ActionabilityReport::from_checks(checks, facts.clone())
}
