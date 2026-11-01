use crate::ax::write_outcome::{native_write_outcome, NativeWriteKind, Settable};
use async_trait::async_trait;
use cua_driver_core::mutation::{ChainResult, Postcondition, PriorInput, Reason, WriteOutcome};
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::Value;
use std::sync::Arc;

use super::ToolState;

pub struct TypeTextCharsTool {
    state: Arc<ToolState>,
}

impl TypeTextCharsTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "type_text_chars".into(),
        description: "Type text character-by-character with a configurable inter-character delay. \
            Useful for apps that miss keystrokes when characters arrive too quickly. \
            Otherwise identical to type_text (CGEvent, no focus steal).".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["pid", "text"],
            "properties": {
                "pid":           { "type": "integer", "description": "Target process ID." },
                "text":          { "type": "string",  "description": "Text to type." },
                "delay_ms":      { "type": "integer", "description": "Milliseconds between characters (default 30)." },
                "window_id":     { "type": "integer", "description": "Window ID for element focus. Optional when element_token is supplied." },
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "type_chars_only": { "type": "boolean", "description": "Skip AX focus, type directly. Default false." }
            },
            "additionalProperties": false
        }),
        read_only: false,
        destructive: true,
        idempotent: false,
        open_world: true,
    })
}

fn type_chars_outcome(
    result: Result<anyhow::Result<()>, tokio::task::JoinError>,
    text_len: usize,
    delay_ms: u64,
) -> ToolResult {
    match result {
        Ok(Ok(())) => ToolResult::text(format!(
            "Typed {text_len} character(s) with {delay_ms}ms delay."
        ))
        .with_structured(serde_json::json!({
            "path": "key_events",
            "characters": text_len,
            "verified": false,
            "effect": "unverifiable"
        })),
        Ok(Err(e)) => super::type_text::text_delivery_failure("Type text chars failed", &e),
        Err(e) => ToolResult::error(format!(
            "Character worker failed: {e}; delivery is unknown. Read fresh state before retrying."
        ))
        .with_disposition(cua_driver_contract::ErrorDisposition::SUSPECTED_NOOP),
    }
}

enum FocusAttempt {
    Native(Settable, i32),
    WorkerFailed(String),
}

fn prior_for_key_entry(entry: crate::input::skylight::KeyEntry) -> PriorInput {
    if entry.records_posted > 0 || entry.records_uncertain || entry.ax_write {
        PriorInput::MayHaveDelivered
    } else {
        PriorInput::None
    }
}

// This boundary also lets the character sender share the focus decision with
// tests without posting native events.
fn focus_failure(attempt: FocusAttempt, prior: PriorInput) -> Option<ToolResult> {
    let (outcome, message) = match attempt {
        FocusAttempt::Native(settable, error) => (
            native_write_outcome(NativeWriteKind::SetAttribute, true, &settable, error),
            format!("AXFocused returned error {error}; no characters were sent."),
        ),
        FocusAttempt::WorkerFailed(error) => (
            WriteOutcome::StopUncertain(Reason::Transport),
            format!("Focus worker failed: {error}; no characters were sent. Read fresh state before retrying."),
        ),
    };
    if outcome == WriteOutcome::Delivered {
        return None;
    }
    let chain = ChainResult::from_recorded(vec![outcome], prior)
        .expect("one focus write is a valid recorded chain");
    let effect = if chain.no_input() {
        "refused"
    } else {
        "unverifiable"
    };
    let mut result = ToolResult::error(message).with_structured(serde_json::json!({
        "code": "not_editable", "effect": effect, "keys_sent": false,
    }));
    if let Some(disposition) = chain.error_disposition(Postcondition::Indeterminate, false) {
        result = result.with_disposition(disposition);
    }
    Some(result)
}

fn prior_with_hand_back(
    prior: PriorInput,
    hand_back: Option<crate::input::skylight::HandBack>,
) -> PriorInput {
    use crate::input::skylight::HandBack;
    if hand_back.is_some_and(|h| h != HandBack::NotMoved) {
        PriorInput::MayHaveDelivered
    } else {
        prior
    }
}

fn append_scope_disclosure(
    result: &mut ToolResult,
    entry: crate::input::skylight::KeyEntry,
    hand_back: Option<crate::input::skylight::HandBack>,
) {
    let disclosure = crate::input::skylight::scope_disclosure(entry, hand_back);
    if !disclosure.is_empty() {
        result
            .content
            .push(cua_driver_core::protocol::Content::text(disclosure));
    }
}

fn scope_entry_failure(error: crate::input::skylight::ScopeEntryError) -> ToolResult {
    let prior = prior_with_hand_back(prior_for_key_entry(error.entry), error.hand_back);
    let outcome = WriteOutcome::StopNotDelivered(Reason::UnconfirmedAlive);
    let chain = ChainResult::from_recorded(vec![outcome], prior).expect("one scope result");
    let effect = if chain.no_input() {
        "refused"
    } else {
        "unverifiable"
    };
    let mut result = ToolResult::error(format!(
        "Could not prepare the key window: {}; no characters were sent.",
        error.reason
    ))
    .with_structured(
        serde_json::json!({"code": super::WINDOW_NOT_KEY, "effect": effect, "keys_sent": false}),
    );
    if let Some(disposition) = chain.error_disposition(Postcondition::Indeterminate, false) {
        result = result.with_disposition(disposition);
    }
    append_scope_disclosure(&mut result, error.entry, error.hand_back);
    result
}

async fn acquire_chars_key_scope(
    pid: i32,
    window_id: Option<u32>,
) -> Result<Option<crate::input::skylight::WindowKeyScope>, ToolResult> {
    use crate::input::skylight;
    let Some(wid) = window_id else {
        return Ok(None);
    };
    super::key_post_preflight(skylight::is_available)?;
    let result = crate::goat::action_latency::spawn_blocking(move || {
        let _span =
            crate::goat::action_latency::span(crate::goat::action_latency::Stage::KeyScopePrime);
        if skylight::is_focus_without_raise_available() {
            skylight::enter_exact_window_key_inline_detailed(pid, wid).map(Some)
        } else {
            Ok(None)
        }
    })
    .await;
    match result {
        Ok(Ok(scope)) => Ok(scope),
        Ok(Err(error)) => Err(scope_entry_failure(error)),
        Err(error) => Err(focus_failure(
            FocusAttempt::WorkerFailed(error.to_string()),
            PriorInput::MayHaveDelivered,
        )
        .expect("worker failure is uncertain")),
    }
}

#[async_trait]
impl Tool for TypeTextCharsTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let pid = match args.require_i32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let text_raw = match args.require_str("text") {
            Ok(v) => v,
            Err(e) => return e,
        };
        // Same trailing-protocol-tag scrub as TypeTextTool — see
        // cua_driver_core::text_sanitize for rationale.
        let text = cua_driver_core::text_sanitize::strip_trailing_agent_protocol_tags(&text_raw)
            .into_owned();
        let delay_ms = args.u64_or("delay_ms", 30);
        let window_id_arg = args.opt_u64("window_id");
        let resolved = match self.state.snapshots.resolve(pid, &args) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (_, window_id, element_guard) = resolved.into_parts(window_id_arg);
        let window_id = match super::native_window_id(window_id) {
            Ok(window_id) => window_id,
            Err(error) => return error,
        };
        let type_chars_only = args.bool_or("type_chars_only", false);

        let element_ptr: Option<usize> = element_guard.as_ref().map(|g| g.as_ptr());

        // ── Exact-target background gate (macOS background input v1) ──
        // This tool is always background CGEvent keystrokes — process-scoped
        // transport with no semantic AX rung. A window-addressed request must
        // prove exact delivery before any event is posted; there is no
        // semantic fallback here, so a refusal is final.
        let _mutation_lease = if let Some(wid) = window_id {
            match super::gate_background_window_action(
                pid,
                wid,
                element_ptr,
                super::window_keyboard_action(
                    cua_driver_core::background_input::BackgroundAction::InsertText,
                ),
            )
            .await
            {
                Ok(lease) => Some(lease),
                Err(refusal_result) => return refusal_result,
            }
        } else {
            None
        };

        let mut key_scope = match acquire_chars_key_scope(pid, window_id).await {
            Ok(scope) => scope,
            Err(refusal) => return refusal,
        };

        // Pre-focus element if requested.
        if !type_chars_only {
            if let Some(guard) = element_guard.as_ref().cloned() {
                let attempt = crate::goat::action_latency::spawn_blocking(move || {
                    let settable = unsafe {
                        crate::ax::write_outcome::query_settable(guard.as_ptr() as _, "AXFocused")
                    };
                    let error = crate::input::ax_actions::focus_element_raw(guard.as_ptr());
                    FocusAttempt::Native(settable, error)
                })
                .await;
                let attempt =
                    attempt.unwrap_or_else(|error| FocusAttempt::WorkerFailed(error.to_string()));
                let mut prior = key_scope
                    .as_ref()
                    .map(|scope| prior_for_key_entry(scope.entry()))
                    .unwrap_or(PriorInput::None);
                let disclosure = if !matches!(&attempt, FocusAttempt::Native(_, error) if *error == crate::ax::bindings::kAXErrorSuccess)
                {
                    let scope = key_scope.take();
                    let entry = scope
                        .as_ref()
                        .map(|scope| scope.entry())
                        .unwrap_or_default();
                    let hand_back = scope.map(|scope| scope.finish());
                    prior = prior_with_hand_back(prior, hand_back);
                    Some((entry, hand_back))
                } else {
                    None
                };
                if let Some(mut failure) = focus_failure(attempt, prior) {
                    let (entry, hand_back) = disclosure.expect("failed focus ended the scope");
                    append_scope_disclosure(&mut failure, entry, hand_back);
                    return failure;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
        drop(element_guard);

        let text_len = text.chars().count();
        let result = crate::goat::action_latency::spawn_blocking(move || {
            crate::input::keyboard::type_text_with_delay(pid, &text, delay_ms)
        })
        .await;

        type_chars_outcome(result, text_len, delay_ms)
    }
}

#[cfg(test)]
mod u32_outcome_tests {
    use super::*;
    use crate::ax::bindings::{kAXErrorAPIDisabled, kAXErrorCannotComplete};

    #[test]
    fn uncertain_focus_prevents_all_character_posts() {
        let mut posts = 0;
        let failure = focus_failure(
            FocusAttempt::Native(Settable::Yes, kAXErrorCannotComplete),
            PriorInput::None,
        );
        if failure.is_none() {
            posts += 1;
        }
        assert_eq!(posts, 0, "uncertain focus must prevent all character posts");
        assert_eq!(
            failure.unwrap().structured_content.unwrap()["effect"],
            "unverifiable"
        );
        let worker = focus_failure(
            FocusAttempt::WorkerFailed("join failed".into()),
            PriorInput::None,
        )
        .unwrap();
        assert_eq!(worker.structured_content.unwrap()["effect"], "unverifiable");
        assert_eq!(
            worker.disposition.unwrap(),
            cua_driver_contract::ErrorDisposition::SUSPECTED_NOOP
        );
        assert!(focus_failure(
            FocusAttempt::Native(Settable::Yes, crate::ax::bindings::kAXErrorSuccess),
            PriorInput::None
        )
        .is_none());
    }

    #[test]
    fn focus_denied_after_key_entry_preserves_prior_input() {
        let mut posts = 0;
        let prior = prior_for_key_entry(crate::input::skylight::KeyEntry {
            records_posted: 1,
            ..Default::default()
        });
        let failure = focus_failure(
            FocusAttempt::Native(Settable::Yes, kAXErrorAPIDisabled),
            prior,
        );
        if failure.is_none() {
            posts += 1;
        }
        assert_eq!(posts, 0, "denied focus must prevent all character posts");
        assert_eq!(
            failure.unwrap().structured_content.unwrap()["effect"],
            "unverifiable"
        );
        let entry = crate::input::skylight::KeyEntry {
            records_uncertain: true,
            ..Default::default()
        };
        let result = scope_entry_failure(crate::input::skylight::ScopeEntryError {
            reason: "denied".into(),
            entry,
            hand_back: Some(crate::input::skylight::HandBack::Deferred),
        });
        assert_eq!(result.structured_content.unwrap()["effect"], "unverifiable");
        assert_eq!(
            result.disposition.unwrap(),
            cua_driver_contract::ErrorDisposition::SUSPECTED_NOOP
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_key_events_include_the_producer_effect_contract() {
        let result = type_chars_outcome(Ok(Ok(())), 3, 17);
        let structured = result.structured_content.expect("structured result");
        assert_eq!(structured["path"], "key_events");
        assert_eq!(structured["characters"], 3);
        assert_eq!(structured["verified"], false);
        assert_eq!(structured["effect"], "unverifiable");
    }

    #[test]
    fn delivery_errors_do_not_get_a_success_payload() {
        let result = type_chars_outcome(Ok(Err(anyhow::anyhow!("boom"))), 3, 17);
        assert_eq!(result.is_error, Some(true));
        assert!(result
            .structured_content
            .as_ref()
            .is_none_or(|structured| structured.get("characters").is_none()));
    }
}
