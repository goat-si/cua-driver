use super::{ElementFacts, Occluder};
use crate::protocol::ToolResult;
use serde::Serialize;
use serde_json::json;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Transient { reason: String, unknown: bool },
    Terminal { code: &'static str, reason: String },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckStatus {
    Pass,
    Transient { unknown: bool },
    Terminal { code: &'static str },
}
#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub name: &'static str,
    pub status: CheckStatus,
    pub reason: String,
    pub occluder: Option<Occluder>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DispositionIntent {
    NotDeliveredSafe,
}
#[derive(Debug, Clone, PartialEq)]
pub struct ActionabilityReport {
    pub verdict: Verdict,
    pub checks: Vec<Check>,
    pub waited_ms: u64,
    pub samples: u32,
    pub last_facts: Option<ElementFacts>,
    pub disposition: Option<DispositionIntent>,
}

impl ActionabilityReport {
    pub(crate) fn from_checks(checks: Vec<Check>, facts: ElementFacts) -> Self {
        let terminal = checks.iter().find_map(|check| match check.status {
            CheckStatus::Terminal { code } => Some(Verdict::Terminal {
                code,
                reason: check.reason.clone(),
            }),
            _ => None,
        });
        let transient = checks.iter().find_map(|check| match check.status {
            CheckStatus::Transient { unknown } => Some(Verdict::Transient {
                reason: check.reason.clone(),
                unknown,
            }),
            _ => None,
        });
        Self {
            verdict: terminal.or(transient).unwrap_or(Verdict::Pass),
            checks,
            waited_ms: 0,
            samples: 1,
            last_facts: Some(facts),
            disposition: None,
        }
    }
    /// Converts a terminal report into the public refusal result.
    pub fn refusal_result(&self) -> Option<ToolResult> {
        match &self.verdict {
            Verdict::Terminal { code, reason } => {
                let message = format!("{code}: {reason} (waited {} ms)", self.waited_ms);
                let mut result = ToolResult::error(&message).with_structured(json!({
                    "code": "element_not_actionable",
                    "effect": "refused",
                    "refusal": {"code": "element_not_actionable", "message": message}
                }));
                if self.disposition == Some(DispositionIntent::NotDeliveredSafe) {
                    result = result
                        .with_disposition(cua_driver_contract::ErrorDisposition::NOT_DELIVERED);
                }
                Some(result)
            }
            _ => None,
        }
    }
}
