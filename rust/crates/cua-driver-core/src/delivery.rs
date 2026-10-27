//! Per-call input delivery mode and the shared `background_unavailable`
//! refusal envelope.
//!
//! Every platform accepts the same two `delivery_mode` rungs, parsed the same
//! way, and refuses an impossible background delivery with the same
//! foreground-escalation contract. Platform adapters keep only the
//! platform-specific reason text and fields.

use serde_json::{json, Value};

use crate::protocol::ToolResult;

/// Input delivery modality: the agent-selected rung of the
/// best-effort-background ladder, passed per call (never a stored or
/// configured setting).
///
/// - `Background` (default): inject without activating or raising the target.
///   When the platform cannot do that for the target, the tool refuses with
///   [`background_unavailable_result`] instead of fronting.
/// - `Foreground`: activate the target for the action. This is the agent's
///   explicit last resort.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum DeliveryMode {
    #[default]
    Background,
    Foreground,
}

impl DeliveryMode {
    /// Parse the per-call `delivery_mode` argument. Anything other than an
    /// explicit case-insensitive `"foreground"` resolves to `Background`, so
    /// an omitted, garbage, or removed legacy value (`"auto"`) never silently
    /// fronts.
    pub fn parse(arg: Option<&str>) -> Self {
        match arg {
            Some(s) if s.eq_ignore_ascii_case("foreground") => Self::Foreground,
            _ => Self::Background,
        }
    }

    /// Parse from a tool's JSON args, reading the `delivery_mode` field.
    pub fn from_args(args: &Value) -> Self {
        Self::parse(args.get("delivery_mode").and_then(Value::as_str))
    }

    pub fn is_foreground(self) -> bool {
        matches!(self, Self::Foreground)
    }
}

/// Environment variable an operator sets when starting the daemon to allow a
/// brief foreground step (trycua/cua#5478). Off by default.
pub const BRIEF_FOREGROUND_ENV: &str = "CUA_DRIVER_ALLOW_BRIEF_FOREGROUND";

/// Whether the operator allowed a brief foreground step for actions that
/// cannot finish in the background.
///
/// When it is on, a tool whose background attempt hits a known platform limit
/// (today: a macOS drag-and-drop session in a native window) repeats only that
/// step in the foreground, then hands the frontmost app and the pointer back,
/// and says so in its result. It is read from the daemon's environment, which
/// the agent can't change, and a call that passes `delivery_mode:"background"`
/// explicitly is never escalated.
pub fn brief_foreground_allowed() -> bool {
    brief_foreground_flag(std::env::var(BRIEF_FOREGROUND_ENV).ok().as_deref())
}

/// Parse [`BRIEF_FOREGROUND_ENV`]: only `1`, `true`, `yes` or `on`
/// (case-insensitive) turn it on.
pub fn brief_foreground_flag(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// Whether the call itself asked for background delivery by name, which opts
/// it out of the brief foreground step.
pub fn background_requested_explicitly(args: &Value) -> bool {
    args.get("delivery_mode")
        .and_then(Value::as_str)
        .is_some_and(|mode| mode.eq_ignore_ascii_case("background"))
}

/// The `suggestion` every background refusal carries.
pub const FOREGROUND_RETRY_SUGGESTION: &str =
    "Retry this action with delivery_mode:\"foreground\".";

/// Build a structured background-delivery refusal.
///
/// The result is an error with `message` as its text. Its structured content
/// holds `code`, the shared [`FOREGROUND_RETRY_SUGGESTION`], and an
/// `escalation` that recommends `foreground` for `escalation_reason`, plus
/// every field of the `fields` object. `fields` cannot override those
/// contract keys.
pub fn background_unavailable_result(
    message: impl Into<String>,
    code: &str,
    escalation_reason: impl Into<String>,
    fields: Value,
) -> ToolResult {
    background_unavailable_result_with_suggestion(
        message,
        code,
        escalation_reason,
        fields,
        FOREGROUND_RETRY_SUGGESTION,
    )
}

/// Like [`background_unavailable_result`], but `suggestion` replaces the
/// foreground retry text. Use it when a working background route exists, so the
/// refusal leads the caller to that route instead of implying the task is
/// blocked. The foreground `escalation` is still included.
pub fn background_unavailable_result_with_suggestion(
    message: impl Into<String>,
    code: &str,
    escalation_reason: impl Into<String>,
    fields: Value,
    suggestion: &str,
) -> ToolResult {
    let mut structured = match fields {
        Value::Object(fields) => fields,
        _ => serde_json::Map::new(),
    };
    structured.insert("code".to_owned(), json!(code));
    structured.insert("suggestion".to_owned(), json!(suggestion));
    structured.insert(
        "escalation".to_owned(),
        json!({
            "recommended": "foreground",
            "reason": escalation_reason.into(),
        }),
    );
    ToolResult::error(message).with_structured(Value::Object(structured))
}

/// Default minimum physical-input idle window (in milliseconds) required
/// before an explicit `delivery_mode:"foreground"` action may front a window
/// or inject into the global HID/SendInput stream.
pub const DEFAULT_FOREGROUND_IDLE_THRESHOLD_MS: u64 = 1_000;

/// Optional daemon environment override for `DEFAULT_FOREGROUND_IDLE_THRESHOLD_MS`.
/// Setting this to `0` disables the foreground physical-idle preflight gate.
pub const FOREGROUND_IDLE_THRESHOLD_ENV: &str = "CUA_DRIVER_FOREGROUND_IDLE_MS";

/// Stable error code returned when `delivery_mode:"foreground"` is refused
/// because physical keyboard/mouse input occurred within the idle threshold.
pub const FOREGROUND_USER_ACTIVE_CODE: &str = "foreground_user_active";

/// Outcome of checking whether physical user input is idle before a foreground
/// delivery step.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PhysicalInputIdleGate {
    /// Physical input has been idle for at least `threshold_ms` (or the
    /// threshold is disabled / no contradicting sensor reading exists).
    Idle {
        idle_ms: Option<u64>,
        threshold_ms: u64,
    },
    /// The user interacted with physical input hardware within `threshold_ms`.
    UserActive { idle_ms: u64, threshold_ms: u64 },
}

/// Parse `FOREGROUND_IDLE_THRESHOLD_ENV` or fall back to
/// `DEFAULT_FOREGROUND_IDLE_THRESHOLD_MS`. Invalid strings keep the default.
pub fn resolve_foreground_idle_threshold_ms(env_val: Option<&str>) -> u64 {
    env_val
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_FOREGROUND_IDLE_THRESHOLD_MS)
}

/// Read the active foreground idle threshold from the daemon environment.
pub fn foreground_idle_threshold_ms() -> u64 {
    resolve_foreground_idle_threshold_ms(
        std::env::var(FOREGROUND_IDLE_THRESHOLD_ENV).ok().as_deref(),
    )
}

/// Compute elapsed milliseconds between two 32-bit OS tick counters
/// (such as Windows `GetTickCount()` and `LASTINPUTINFO::dwTime`), handling
/// 49.7-day `u32` wraparound via wrapping subtraction.
pub fn elapsed_idle_ms_from_ticks(now_ticks: u32, last_input_ticks: u32) -> u64 {
    u64::from(now_ticks.wrapping_sub(last_input_ticks))
}

/// Slack around the driver's own injection window. Tick counters advance in
/// steps of about 16 ms, and the OS stamps injected input as it processes it.
pub const OWN_INPUT_TICK_SLACK_MS: u64 = 50;

/// Idle time since physical input, from the OS's last-input tick, with the
/// driver's own injected input set aside. Windows `GetLastInputInfo` also
/// counts `SendInput`, so the driver's last foreground action would otherwise
/// look like the person typing and refuse the next one. `own_input` is the
/// tick span of the driver's latest `SendInput`. When the last input falls
/// inside it, there is no evidence of physical input and this returns `None`.
pub fn idle_ms_excluding_own_input(
    now_ticks: u32,
    last_input_ticks: u32,
    own_input: Option<(u32, u32)>,
) -> Option<u64> {
    let idle = elapsed_idle_ms_from_ticks(now_ticks, last_input_ticks);
    if let Some((start, end)) = own_input {
        let since_start = elapsed_idle_ms_from_ticks(now_ticks, start);
        let since_end = elapsed_idle_ms_from_ticks(now_ticks, end);
        if idle <= since_start + OWN_INPUT_TICK_SLACK_MS
            && idle + OWN_INPUT_TICK_SLACK_MS >= since_end
        {
            return None;
        }
    }
    Some(idle)
}

/// Pure decision for whether a foreground action may proceed given an observed
/// physical-input idle duration in milliseconds.
pub fn evaluate_physical_input_idle(
    observed_idle_ms: Option<u64>,
    threshold_ms: u64,
) -> PhysicalInputIdleGate {
    if threshold_ms == 0 {
        return PhysicalInputIdleGate::Idle {
            idle_ms: observed_idle_ms,
            threshold_ms,
        };
    }
    match observed_idle_ms {
        Some(idle_ms) if idle_ms < threshold_ms => PhysicalInputIdleGate::UserActive {
            idle_ms,
            threshold_ms,
        },
        idle_ms => PhysicalInputIdleGate::Idle {
            idle_ms,
            threshold_ms,
        },
    }
}

/// Build the structured refusal returned when `delivery_mode:"foreground"`
/// is blocked by recent physical user input.
pub fn foreground_user_active_result(idle_ms: u64, threshold_ms: u64, fields: Value) -> ToolResult {
    let mut structured = match fields {
        Value::Object(fields) => fields,
        _ => serde_json::Map::new(),
    };
    let suggestion = "Wait until physical keyboard/mouse input is idle before retrying delivery_mode:\"foreground\", or use a background-compatible route.";
    structured.insert("code".to_owned(), json!(FOREGROUND_USER_ACTIVE_CODE));
    structured.insert("effect".to_owned(), json!("refused"));
    structured.insert("dispatched".to_owned(), json!(false));
    structured.insert("idle_ms".to_owned(), json!(idle_ms));
    structured.insert("required_idle_ms".to_owned(), json!(threshold_ms));
    structured.insert("suggestion".to_owned(), json!(suggestion));
    ToolResult::error(format!(
        "Foreground delivery refused ({FOREGROUND_USER_ACTIVE_CODE}): physical user input was detected {idle_ms} ms ago (requires at least {threshold_ms} ms of idle time). {suggestion}"
    ))
    .with_structured(Value::Object(structured))
}

#[cfg(test)]
mod tests {
    #[test]
    fn brief_foreground_is_off_unless_explicitly_enabled() {
        for off in [
            None,
            Some(""),
            Some("0"),
            Some("false"),
            Some("no"),
            Some("maybe"),
        ] {
            assert!(!super::brief_foreground_flag(off), "{off:?}");
        }
        for on in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(super::brief_foreground_flag(Some(on)), "{on:?}");
        }
    }

    #[test]
    fn explicit_background_opts_out_of_the_brief_foreground_step() {
        use serde_json::json;
        assert!(super::background_requested_explicitly(
            &json!({"delivery_mode": "Background"})
        ));
        assert!(!super::background_requested_explicitly(&json!({})));
        assert!(!super::background_requested_explicitly(
            &json!({"delivery_mode": "foreground"})
        ));
    }

    use super::*;

    #[test]
    fn delivery_mode_parse_fronts_only_on_explicit_foreground() {
        for (args, expected) in [
            (
                json!({"delivery_mode": "background"}),
                DeliveryMode::Background,
            ),
            (
                json!({"delivery_mode": "foreground"}),
                DeliveryMode::Foreground,
            ),
            (
                json!({"delivery_mode": "Foreground"}),
                DeliveryMode::Foreground,
            ),
            (
                json!({"delivery_mode": "FOREGROUND"}),
                DeliveryMode::Foreground,
            ),
            (json!({}), DeliveryMode::Background),
            (
                json!({"delivery_mode": "garbage"}),
                DeliveryMode::Background,
            ),
            (json!({"delivery_mode": "auto"}), DeliveryMode::Background),
            (json!({"delivery_mode": null}), DeliveryMode::Background),
            (json!({"delivery_mode": 1}), DeliveryMode::Background),
        ] {
            let mode = DeliveryMode::from_args(&args);
            assert_eq!(mode, expected, "{args}");
            assert_eq!(mode.is_foreground(), expected == DeliveryMode::Foreground);
        }
    }

    #[test]
    fn a_custom_suggestion_replaces_only_the_foreground_retry_text() {
        let result = background_unavailable_result_with_suggestion(
            "refused",
            "background_unavailable",
            "why",
            json!({}),
            "use get_window_state",
        );
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["suggestion"], "use get_window_state");
        assert_eq!(structured["code"], "background_unavailable");
        assert_eq!(structured["escalation"]["recommended"], "foreground");
    }

    #[test]
    fn background_unavailable_result_carries_the_foreground_escalation() {
        let result = background_unavailable_result(
            "Background delivery is not available.",
            "background_occluded",
            "the target is occluded",
            json!({
                "event_kind": "mouse_click",
                "code": "forged",
                "suggestion": "forged",
            }),
        );
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content,
            Some(json!({
                "code": "background_occluded",
                "event_kind": "mouse_click",
                "suggestion": "Retry this action with delivery_mode:\"foreground\".",
                "escalation": {
                    "recommended": "foreground",
                    "reason": "the target is occluded",
                },
            }))
        );
        match &result.content[0] {
            crate::protocol::Content::Text { text, .. } => {
                assert_eq!(text, "Background delivery is not available.")
            }
            _ => panic!("expected text content"),
        }
    }

    #[test]
    fn physical_input_idle_gate_refuses_when_user_was_recently_active() {
        assert_eq!(
            evaluate_physical_input_idle(Some(250), 1_000),
            PhysicalInputIdleGate::UserActive {
                idle_ms: 250,
                threshold_ms: 1_000,
            }
        );
        assert_eq!(
            evaluate_physical_input_idle(Some(1_000), 1_000),
            PhysicalInputIdleGate::Idle {
                idle_ms: Some(1_000),
                threshold_ms: 1_000,
            }
        );
        assert_eq!(
            evaluate_physical_input_idle(None, 1_000),
            PhysicalInputIdleGate::Idle {
                idle_ms: None,
                threshold_ms: 1_000,
            }
        );
        assert_eq!(
            evaluate_physical_input_idle(Some(10), 0),
            PhysicalInputIdleGate::Idle {
                idle_ms: Some(10),
                threshold_ms: 0,
            }
        );
    }

    #[test]
    fn foreground_idle_threshold_env_parser_handles_defaults_and_zero() {
        assert_eq!(
            resolve_foreground_idle_threshold_ms(None),
            DEFAULT_FOREGROUND_IDLE_THRESHOLD_MS
        );
        assert_eq!(
            resolve_foreground_idle_threshold_ms(Some("invalid")),
            DEFAULT_FOREGROUND_IDLE_THRESHOLD_MS
        );
        assert_eq!(resolve_foreground_idle_threshold_ms(Some("0")), 0);
        assert_eq!(resolve_foreground_idle_threshold_ms(Some(" 2500 ")), 2_500);
    }

    #[test]
    fn tick_wraparound_computes_exact_elapsed_idle_ms() {
        assert_eq!(elapsed_idle_ms_from_ticks(5_000, 4_200), 800);
        assert_eq!(elapsed_idle_ms_from_ticks(100, u32::MAX - 49), 150);
    }

    #[test]
    fn the_drivers_own_injected_input_is_not_physical_input() {
        // Driver injected from tick 10_000 to 10_016; now is 10_200.
        let own = Some((10_000, 10_016));
        // Last input inside that span (or just after, as the OS stamps it).
        assert_eq!(idle_ms_excluding_own_input(10_200, 10_016, own), None);
        assert_eq!(idle_ms_excluding_own_input(10_200, 10_060, own), None);
        // Input after the driver's injection is the person's.
        assert_eq!(idle_ms_excluding_own_input(10_200, 10_150, own), Some(50));
        // Input well before it predates the driver's and still counts.
        assert_eq!(idle_ms_excluding_own_input(10_200, 9_000, own), Some(1_200));
        // Nothing injected yet: the plain elapsed time.
        assert_eq!(idle_ms_excluding_own_input(10_200, 10_016, None), Some(184));
        // Tick wraparound between injection and now.
        let wrapped = Some((u32::MAX - 20, u32::MAX - 4));
        assert_eq!(
            idle_ms_excluding_own_input(100, u32::MAX - 4, wrapped),
            None
        );
        assert_eq!(idle_ms_excluding_own_input(100, 90, wrapped), Some(10));
    }

    #[test]
    fn foreground_user_active_result_carries_refusal_contract() {
        let res = foreground_user_active_result(180, 1_000, json!({"pid": 42, "window_id": 7}));
        assert_eq!(res.is_error, Some(true));
        let structured = res.structured_content.unwrap();
        assert_eq!(structured["code"], FOREGROUND_USER_ACTIVE_CODE);
        assert_eq!(structured["effect"], "refused");
        assert_eq!(structured["dispatched"], false);
        assert_eq!(structured["idle_ms"], 180);
        assert_eq!(structured["required_idle_ms"], 1_000);
        assert_eq!(structured["pid"], 42);
        assert_eq!(structured["window_id"], 7);
    }
}
