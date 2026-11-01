// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

use crate::{schema_settings, ActionHit, ActionPoint, CaptureScope, EscalationReason, Platform};
use schemars::JsonSchema;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// Evidence about input sent to a target application, not other side effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputDelivery {
    NotDelivered,
    DeliveryUncertain,
    DeliveredUnverified,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrySafety {
    Safe,
    Unsafe,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorDisposition {
    pub delivery: InputDelivery,
    pub retry: RetrySafety,
}

impl ErrorDisposition {
    pub const NOT_DELIVERED: Self = Self {
        delivery: InputDelivery::NotDelivered,
        retry: RetrySafety::Safe,
    };
    pub const UNCERTAIN: Self = Self {
        delivery: InputDelivery::DeliveryUncertain,
        retry: RetrySafety::Unknown,
    };
    pub const SUSPECTED_NOOP: Self = Self {
        delivery: InputDelivery::DeliveryUncertain,
        retry: RetrySafety::Unsafe,
    };
    pub const DELIVERED: Self = Self {
        delivery: InputDelivery::DeliveredUnverified,
        retry: RetrySafety::Unsafe,
    };
    pub const UNKNOWN: Self = Self {
        delivery: InputDelivery::Unknown,
        retry: RetrySafety::Unknown,
    };

    pub fn is_valid(self) -> bool {
        matches!(
            (self.delivery, self.retry),
            (InputDelivery::NotDelivered, RetrySafety::Safe)
                | (
                    InputDelivery::DeliveryUncertain,
                    RetrySafety::Unsafe | RetrySafety::Unknown
                )
                | (InputDelivery::DeliveredUnverified, RetrySafety::Unsafe)
                | (InputDelivery::Unknown, RetrySafety::Unknown)
        )
    }
}

/// Derive only from evidence common to all tools; refusal markers and codes
/// alone do not prove that input was not dispatched.
pub fn derive_error_disposition(structured: &Value, read_only: Option<bool>) -> ErrorDisposition {
    match structured.get("effect").and_then(Value::as_str) {
        Some("partial" | "unverifiable") => ErrorDisposition::DELIVERED,
        Some("suspected_noop") => ErrorDisposition::SUSPECTED_NOOP,
        _ if read_only == Some(true) => ErrorDisposition::NOT_DELIVERED,
        _ => ErrorDisposition::UNKNOWN,
    }
}

#[cfg(test)]
mod disposition_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_documented_pairs_validate() {
        use InputDelivery::*;
        use RetrySafety::*;
        for delivery in [
            NotDelivered,
            DeliveryUncertain,
            DeliveredUnverified,
            InputDelivery::Unknown,
        ] {
            for retry in [Safe, Unsafe, RetrySafety::Unknown] {
                let disposition = ErrorDisposition { delivery, retry };
                let valid = matches!(
                    (delivery, retry),
                    (NotDelivered, Safe)
                        | (DeliveryUncertain, Unsafe | RetrySafety::Unknown)
                        | (DeliveredUnverified, Unsafe)
                        | (InputDelivery::Unknown, RetrySafety::Unknown)
                );
                assert_eq!(disposition.is_valid(), valid, "{disposition:?}");
            }
        }
        assert!(serde_json::from_value::<ErrorDisposition>(
            json!({"delivery":"future", "retry":"safe"})
        )
        .is_err());
    }

    #[test]
    fn derives_only_from_supported_evidence() {
        for (payload, read_only, expected) in [
            (json!({}), Some(true), ErrorDisposition::NOT_DELIVERED),
            (
                json!({"effect":"partial"}),
                None,
                ErrorDisposition::DELIVERED,
            ),
            (
                json!({"effect":"unverifiable"}),
                None,
                ErrorDisposition::DELIVERED,
            ),
            (
                json!({"effect":"suspected_noop"}),
                None,
                ErrorDisposition::SUSPECTED_NOOP,
            ),
            (json!({"effect":"refused"}), None, ErrorDisposition::UNKNOWN),
            (
                json!({"code":"permission_denied"}),
                None,
                ErrorDisposition::UNKNOWN,
            ),
            (json!({}), None, ErrorDisposition::UNKNOWN),
        ] {
            assert_eq!(derive_error_disposition(&payload, read_only), expected);
        }
    }

    #[test]
    fn refusal_schema_stays_open_and_unchanged() {
        assert_eq!(
            refusal_envelope_schema(),
            json!({
                "type":"object", "additionalProperties":true,
                "anyOf":[{"required":["refusal"]},{"required":["status"]},{"required":["code"]}]
            })
        );
    }
}

/// Transport-free structured result used by both the live runtime and SDK generation.
pub trait ToolOutput: Serialize + DeserializeOwned + JsonSchema {
    fn validate(&self) -> Result<(), String> {
        Ok(())
    }

    fn output_schema() -> Value {
        output_schema_with_additional_properties::<Self>(true)
    }
}

/// Schema for the refusal payload a tool emits alongside `isError: true`.
///
/// Refusals answer with a diagnostic shape rather than the success shape, and
/// two are in service: `{"status":"refused","refusal":{code,message,detail}}`
/// on the element-token and daemon paths, and `{"code":…,"effect":"refused",…}`
/// on the window-target paths. Both carry tool-specific diagnostic keys, so the
/// envelope stays open — the marker keys are what make it recognisably a
/// refusal rather than a malformed success.
pub fn refusal_envelope_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": true,
        "anyOf": [
            {"required": ["refusal"]},
            {"required": ["status"]},
            {"required": ["code"]},
        ],
    })
}

/// Marker code for structured error envelopes that carry no tool-specific
/// refusal shape.
///
/// The transport and daemon failure paths answer with diagnostics like
/// `{"exit_code": 1}` rather than a tool refusal. That payload satisfies
/// neither arm of [`advertised_output_schema`]: it is missing the success
/// arm's required keys while carrying a key the success arm does not allow,
/// and it has none of the refusal arm's marker keys. Strict MCP clients then
/// reject the whole response, so the error text in `content` never reaches the
/// agent.
pub const TOOL_INVOCATION_FAILED_CODE: &str = "tool_invocation_failed";

/// Keys that make a payload recognisable to the refusal arm of
/// [`advertised_output_schema`].
const REFUSAL_MARKER_KEYS: [&str; 3] = ["refusal", "status", "code"];

/// Whether a payload already satisfies the refusal arm of
/// [`advertised_output_schema`].
pub fn is_refusal_envelope(value: &Value) -> bool {
    value.as_object().is_some_and(has_refusal_marker)
}

fn has_refusal_marker(object: &Map<String, Value>) -> bool {
    REFUSAL_MARKER_KEYS
        .iter()
        .any(|marker| object.contains_key(*marker))
}

/// Guarantee a structured error payload is recognisable as a refusal.
///
/// Inserts [`TOOL_INVOCATION_FAILED_CODE`] when the payload carries none of the
/// refusal marker keys, so any diagnostic an error path invents still validates
/// against the advertised `outputSchema`. Payloads that already carry a marker
/// are returned untouched.
pub fn conforming_error_envelope(structured: Value) -> Value {
    // Both arms require `type: object`, so a non-object diagnostic is kept as a
    // value inside the envelope rather than being emitted as the envelope.
    let mut object = match structured {
        Value::Object(object) => object,
        Value::Null => Map::new(),
        other => {
            let mut object = Map::new();
            object.insert("detail".into(), other);
            object
        }
    };
    if !has_refusal_marker(&object) {
        object.insert(
            "code".into(),
            Value::String(TOOL_INVOCATION_FAILED_CODE.into()),
        );
    }
    Value::Object(object)
}

/// Wrap a success schema into the shape advertised as the MCP `outputSchema`.
///
/// MCP requires every `structuredContent` a tool emits to validate against its
/// advertised `outputSchema` — including payloads that accompany
/// `isError: true`. Advertising the success shape alone made strict clients
/// reject refusals outright, replacing the actionable message the driver had
/// already placed in `content` ("element_token is stale; call get_window_state
/// again to refresh") with an opaque schema-validation error. Agents then had
/// no signal to re-snapshot and fell back to blind pixel clicking.
///
/// The success variant is kept exactly as generated — still closed, so success
/// payloads stay strictly checked — and the refusal envelope joins it as a
/// sibling variant.
pub fn advertised_output_schema(success: Value) -> Value {
    serde_json::json!({ "type": "object", "anyOf": [success, refusal_envelope_schema()] })
}

pub(crate) fn output_schema_with_additional_properties<T: JsonSchema>(
    additional_properties: bool,
) -> Value {
    let mut schema = serde_json::to_value(
        schema_settings()
            .into_generator()
            .into_root_schema_for::<T>(),
    )
    .expect("JSON Schema serializes");
    strip_schema_titles(&mut schema);
    if let Some(object) = schema.as_object_mut() {
        object.insert(
            "additionalProperties".into(),
            Value::Bool(additional_properties),
        );
        object
            .entry("properties")
            .or_insert_with(|| Value::Object(Map::new()));
    }
    schema
}

/// Compact generated schemas by removing the JSON Schema annotations `title`
/// and `description`.
///
/// `properties` and `patternProperties` hold property names as keys, not
/// annotations, so the stripper must recurse into each property schema without
/// touching the map's keys — a tool may legitimately publish a property named
/// `title` (list_windows does), and deleting it silently under-describes the
/// contract.
fn strip_schema_titles(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.remove("title");
            object.remove("description");
            for (key, child) in object.iter_mut() {
                if key == "properties" || key == "patternProperties" {
                    match child {
                        Value::Object(properties) => {
                            properties.values_mut().for_each(strip_schema_titles)
                        }
                        other => strip_schema_titles(other),
                    }
                } else {
                    strip_schema_titles(child);
                }
            }
        }
        Value::Array(values) => values.iter_mut().for_each(strip_schema_titles),
        _ => {}
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum EffectiveScope {
    Window,
    Desktop,
}

impl EffectiveScope {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Window => "window",
            Self::Desktop => "desktop",
        }
    }
}

/// Successful structured result shared by session state and escalation tools.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
pub struct SessionStateOutput {
    pub session: String,
    pub capture_scope: CaptureScope,
    pub effective_scope: EffectiveScope,
    /// Whether this session is authorized to use desktop-scope capture and
    /// actions. This does not report the operating system's lock-screen state.
    pub desktop_capture_authorized: bool,
    /// Compatibility field: this reports whether this session has unlocked
    /// desktop capture scope. It is not an operating-system lock-screen probe.
    pub desktop_unlocked: bool,
    #[schemars(required, schema_with = "nullable_escalation_reason_schema")]
    pub escalation_reason: Option<EscalationReason>,
    #[schemars(required, schema_with = "nullable_string_schema")]
    pub escalation_detail: Option<String>,
}

impl ToolOutput for SessionStateOutput {}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum SessionLifecycleState {
    Active,
    Ending,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum SessionClientKindOutput {
    Cli,
    Direct,
    Mcp,
    PythonSdk,
    TypescriptSdk,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum SessionTransportOutput {
    Cli,
    Daemon,
    McpStdio,
    McpHttp,
}

/// Content-free lifecycle state safe for an ordinary agent transport.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
pub struct SessionOutput {
    /// Sanitized public label, or null for an unnamed implicit session.
    #[schemars(required, schema_with = "nullable_string_schema")]
    pub session: Option<String>,
    pub implicit: bool,
    pub state: SessionLifecycleState,
    pub client_kind: SessionClientKindOutput,
    pub transport: SessionTransportOutput,
    pub cursor_visible: bool,
    pub recording_active: bool,
    pub idle_seconds: u64,
    pub expires_in_seconds: u64,
}

impl ToolOutput for SessionOutput {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
pub struct ListSessionsOutput {
    pub sessions: Vec<SessionOutput>,
    #[schemars(required, schema_with = "nullable_string_schema")]
    pub next_cursor: Option<String>,
}

impl ToolOutput for ListSessionsOutput {}

/// Successful structured result returned by `start_session`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
pub struct StartSessionOutput {
    #[serde(flatten)]
    pub state: SessionStateOutput,
    pub active: bool,
    pub revived: bool,
    /// The `cursor_motion` this call applied, echoed as sent. Absent when the call did not set one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_motion: Option<crate::CursorMotionSelection>,
}

impl ToolOutput for StartSessionOutput {}

/// Successful structured result returned by `end_session`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
pub struct EndSessionOutput {
    pub session: String,
    #[schemars(schema_with = "inactive_schema")]
    pub active: bool,
}

impl ToolOutput for EndSessionOutput {
    fn validate(&self) -> Result<(), String> {
        if self.active {
            Err("active must be false".into())
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
pub struct CursorMotionOutput {
    pub start_handle: f64,
    pub end_handle: f64,
    pub arc_size: f64,
    pub arc_flow: f64,
    pub spring: f64,
    pub glide_duration_ms: f64,
    pub dwell_after_click_ms: f64,
    pub idle_hide_ms: f64,
    pub turn_radius: f64,
    /// Trajectory style. Absent from daemons that predate motion styles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "cursor_motion_style_output_schema")]
    pub style: Option<crate::CursorMotionStyle>,
    /// Move duration model. Absent from daemons that predate motion styles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "cursor_motion_timing_output_schema")]
    pub timing: Option<crate::CursorMotionTiming>,
    /// Effects in use after the style defaults. Absent from daemons that
    /// predate motion styles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effects: Option<crate::CursorMotionEffectsOutput>,
}

// Optional enums advertise the bare string enum (Gemini rejects `null` in
// `enum`); an older daemon simply omits the field.
fn cursor_motion_style_output_schema(
    generator: &mut schemars::SchemaGenerator,
) -> schemars::Schema {
    <crate::CursorMotionStyle as JsonSchema>::json_schema(generator)
}

fn cursor_motion_timing_output_schema(
    generator: &mut schemars::SchemaGenerator,
) -> schemars::Schema {
    <crate::CursorMotionTiming as JsonSchema>::json_schema(generator)
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
pub struct CursorThemeOutput {
    pub id: String,
    pub version: String,
    pub profile: String,
    pub reduced_motion: crate::CursorReducedMotion,
    #[schemars(required, schema_with = "nullable_string_schema")]
    pub fallback: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
pub struct CursorVisualOutput {
    pub requested_action: crate::CursorAction,
    pub resolved_action: crate::CursorAction,
    pub modifiers: Vec<String>,
    pub phase: String,
    pub frame: u64,
    pub preempted_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
pub struct CursorPointOutput {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
pub struct SetAgentCursorEnabledOutput {
    pub session: String,
    pub enabled: bool,
}

impl ToolOutput for SetAgentCursorEnabledOutput {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
pub struct SetAgentCursorMotionOutput {
    pub session: String,
    pub motion: CursorMotionOutput,
}

impl ToolOutput for SetAgentCursorMotionOutput {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
pub struct SetAgentCursorThemeOutput {
    pub session: String,
    pub theme: CursorThemeOutput,
}

impl ToolOutput for SetAgentCursorThemeOutput {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
pub struct GetAgentCursorStateOutput {
    pub session: String,
    pub enabled: bool,
    // Wire contract: the key is always present, and its value is `null` until
    // the session cursor first moves.
    #[schemars(required, schema_with = "nullable_cursor_point_schema")]
    pub position: Option<CursorPointOutput>,
    pub theme: CursorThemeOutput,
    pub visual_state: CursorVisualOutput,
    pub motion: CursorMotionOutput,
}

impl ToolOutput for GetAgentCursorStateOutput {}

fn inactive_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "const": false })
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct DesktopStateOutput {
    #[schemars(schema_with = "platform_schema")]
    pub platform: Platform,
    pub display: String,
    #[schemars(schema_with = "integer_schema")]
    pub screenshot_width: u64,
    #[schemars(schema_with = "integer_schema")]
    pub screenshot_height: u64,
    #[schemars(schema_with = "integer_schema")]
    pub screen_width: u64,
    #[schemars(schema_with = "integer_schema")]
    pub screen_height: u64,
    #[schemars(schema_with = "number_schema")]
    pub scale_factor: f64,
    #[schemars(schema_with = "png_mime_schema")]
    pub screenshot_mime_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "string_schema")]
    pub screenshot_file_path: Option<String>,
    /// Whether the Driver's own overlay pixels (agent cursor, session pill)
    /// were kept out of this capture. Absent from producers that predate the
    /// report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_overlay_capture: Option<AgentOverlayCapture>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

/// How a Driver-owned desktop capture treated Driver-owned overlay pixels.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentOverlayCaptureStatus {
    /// Overlay windows were on screen and were kept out of the pixels.
    Excluded,
    /// No Driver overlay pixels were on screen, so there was nothing to keep out.
    NotPresent,
    /// Overlay pixels may be in the capture; `reason` says why.
    NotExcluded,
}

/// Report attached to desktop captures about Driver-owned overlay pixels.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentOverlayCapture {
    pub status: AgentOverlayCaptureStatus,
    /// Native mechanism that kept the overlay out, when `status` is `excluded`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Why the overlay could not be kept out, when `status` is `not_excluded`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl AgentOverlayCapture {
    pub fn excluded(method: impl Into<String>) -> Self {
        Self {
            status: AgentOverlayCaptureStatus::Excluded,
            method: Some(method.into()),
            reason: None,
        }
    }

    pub fn not_present() -> Self {
        Self {
            status: AgentOverlayCaptureStatus::NotPresent,
            method: None,
            reason: None,
        }
    }

    pub fn not_excluded(reason: impl Into<String>) -> Self {
        Self {
            status: AgentOverlayCaptureStatus::NotExcluded,
            method: None,
            reason: Some(reason.into()),
        }
    }

    /// Whether the capture is free of Driver overlay pixels.
    pub fn is_clean(&self) -> bool {
        self.status != AgentOverlayCaptureStatus::NotExcluded
    }

    fn validate(&self) -> Result<(), String> {
        match (self.status, &self.method, &self.reason) {
            (AgentOverlayCaptureStatus::Excluded, Some(_), None)
            | (AgentOverlayCaptureStatus::NotPresent, None, None)
            | (AgentOverlayCaptureStatus::NotExcluded, None, Some(_)) => Ok(()),
            _ => Err(
                "agent_overlay_capture: excluded requires only method, not_excluded requires \
                 only reason, not_present carries neither"
                    .into(),
            ),
        }
    }
}

impl ToolOutput for DesktopStateOutput {
    fn validate(&self) -> Result<(), String> {
        if self.screenshot_mime_type != "image/png" {
            return Err("screenshot_mime_type must be image/png".into());
        }
        match &self.agent_overlay_capture {
            Some(report) => report.validate(),
            None => Ok(()),
        }
    }
}

fn png_mime_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "const": "image/png" })
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ScreenSizeOutput {
    #[schemars(schema_with = "number_schema")]
    pub width: f64,
    #[schemars(schema_with = "number_schema")]
    pub height: f64,
    #[schemars(schema_with = "number_schema")]
    pub scale_factor: f64,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

impl ToolOutput for ScreenSizeOutput {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct CursorPositionOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "number_schema")]
    pub x: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "number_schema")]
    pub y: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "boolean_schema")]
    pub available: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "string_schema")]
    pub source: Option<String>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

impl ToolOutput for CursorPositionOutput {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
pub struct ClipboardReadOutput {
    pub supported: bool,
    pub types: Vec<String>,
    #[schemars(required, schema_with = "nullable_string_schema")]
    pub text: Option<String>,
    pub privacy_sensitive: bool,
    pub content_redacted_from_telemetry: bool,
}

impl ToolOutput for ClipboardReadOutput {
    fn validate(&self) -> Result<(), String> {
        if !self.supported {
            return Err("successful clipboard reads must be supported".into());
        }
        if !self.privacy_sensitive || !self.content_redacted_from_telemetry {
            return Err("clipboard privacy metadata must remain enabled".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
pub struct ClipboardWriteOutput {
    pub supported: bool,
    pub written_type: String,
    pub types: Vec<String>,
    pub privacy_sensitive: bool,
    pub content_redacted_from_telemetry: bool,
}

impl ToolOutput for ClipboardWriteOutput {
    fn validate(&self) -> Result<(), String> {
        if !self.supported {
            return Err("successful clipboard writes must be supported".into());
        }
        if !matches!(self.written_type.as_str(), "text" | "image" | "file_url") {
            return Err("written_type must be text, image, or file_url".into());
        }
        if !self.privacy_sensitive || !self.content_redacted_from_telemetry {
            return Err("clipboard privacy metadata must remain enabled".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum ActionEffect {
    Confirmed,
    Partial,
    Unverifiable,
    SuspectedNoop,
    Refused,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum ActionRoute {
    Accessibility,
    SyntheticEvents,
    GlobalInput,
    SystemApi,
    Dom,
    TrustedInput,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum ActionDeliveryMode {
    Background,
    Foreground,
    NotApplicable,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct ActionDelivery {
    pub mode: ActionDeliveryMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_count: Option<u32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum ActionEvidenceKind {
    ValueReadback,
    WindowChange,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct ActionEvidence {
    pub kind: ActionEvidenceKind,
    /// Human-readable readback the evidence rests on (what changed, which
    /// popup or window appeared and how to target it). Never request data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Why a `refused` action sent no input, and what to do instead.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct ActionError {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum ActionEscalationTarget {
    Pixel,
    Foreground,
    Page,
    Session,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum ActionEscalationReason {
    RouteUnavailable,
    DeliveryFailed,
    EffectUnconfirmed,
    SuspectedNoop,
    PermissionRequired,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct ActionEscalation {
    pub target: ActionEscalationTarget,
    pub reason: ActionEscalationReason,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum ActionFocusRestorationStatus {
    Restored,
    Abandoned,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum ActionFocusRestorationReason {
    UserInput,
    ThirdApp,
    Timeout,
    Panic,
    Unconfirmed,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct ActionFocusRestoration {
    pub status: ActionFocusRestorationStatus,
    /// Present only with `status: abandoned`. Advertised as a plain string
    /// enum (no null member): the field is omitted when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "ActionFocusRestorationReason")]
    pub reason: Option<ActionFocusRestorationReason>,
    /// The producer reports null when the previous process was unknown.
    #[serde(deserialize_with = "deserialize_prior_pid")]
    #[schemars(required, schema_with = "nullable_prior_pid_schema")]
    pub prior_pid: Option<i32>,
}

fn nullable_prior_pid_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": ["integer", "null"], "format": "int32" })
}

fn deserialize_prior_pid<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<i32>, D::Error> {
    Option::<i32>::deserialize(deserializer)
}

/// Completeness of the observation performed after an input was dispatched.
/// Observed is complete; partial may include surfaces the input did not cause
/// and does not establish the absence of others. Unavailable means observation
/// could not complete. Only observed with an empty list asserts nothing opened.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum OpenedSurfacesStatus {
    Observed,
    Partial,
    Unavailable,
    NotChecked,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum OpenedSurfaceKind {
    Menu,
    Popover,
    Alert,
    Sheet,
    Unknown,
}

/// A target addressable by `get_window_state`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct OpenedSurfaceTarget {
    #[schemars(schema_with = "opened_pid_schema")]
    pub pid: u32,
    #[schemars(schema_with = "opened_window_id_schema")]
    pub window_id: u64,
}

fn opened_pid_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type":"integer", "minimum":1, "maximum":4294967295_u64})
}

fn opened_window_id_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type":"integer", "minimum":1})
}

/// Bounds of a newly observed surface in screen coordinates.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct OpenedSurfaceBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// A surface observed after dispatch. Partial observations may include surfaces
/// the input did not cause. Kind unknown makes no menu, popover, alert, or sheet
/// claim. Embedded AX/UIA surfaces may have no independently addressable target.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct OpenedSurface {
    pub kind: OpenedSurfaceKind,
    /// The owner process, omitted when it could not be established.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "opened_pid_schema")]
    pub owner_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "opened_window_id_schema")]
    pub window_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<OpenedSurfaceTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<OpenedSurfaceBounds>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

// `PartialEq` only: `point` carries floating-point coordinates.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct ActionResult {
    pub effect: ActionEffect,
    pub route: ActionRoute,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<ActionDelivery>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Vec<ActionEvidence>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalation: Option<ActionEscalation>,
    /// The producer's human summary of what happened (resolved points, the
    /// element hit, popups that opened, focus outcome, follow-up calls).
    /// Clients that read only `structuredContent` still get everything the
    /// text content says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_restoration: Option<ActionFocusRestoration>,
    /// Present only with `effect: refused`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ActionError>,
    /// What to do next, in one short sentence. Present when the effect is
    /// `unverifiable` or `suspected_noop`, so an unconfirmed action points at
    /// the next step instead of leaving the agent to guess. Also carries a
    /// suggestion to chain steps with `run_steps` after several direct actions
    /// in a row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// The call that moves the task forward, always safe to run as is (a read,
    /// or a call that keeps the user's window where it is).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_call: Option<ActionNextCall>,
    /// Producer confirmation retained for the macOS drag/right-click readbacks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_changed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_text_range: Option<ActionSelectedTextRange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Legacy macOS readback; false means unconfirmed, not proven absent.
    /// See `opened_surfaces` for the cross-platform observation.
    pub menu_opened: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub menu_items: Option<Vec<String>>,
    /// Surfaces appearing after this input was dispatched. Status absent means
    /// unknown; only observed with an empty list proves none opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opened_surfaces: Option<Vec<OpenedSurface>>,
    /// A failed or deadline-limited observation cannot be `observed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "OpenedSurfacesStatus")]
    pub opened_surfaces_status: Option<OpenedSurfacesStatus>,
    /// The point a pointer action (click, double_click, right_click,
    /// move_cursor, scroll, drag) aimed at, in `coordinate_space`, as its
    /// producer reports it; for drag, the end point. It says where the input
    /// was aimed, not that anything changed there: `effect` and `evidence`
    /// carry the outcome. Absent when the producer reported no point (macOS
    /// reports one for window-points calls, which cannot also name an
    /// element_token).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point: Option<ActionPoint>,
    /// The element the pointer route reported hitting at dispatch time.
    /// Absent when the route reported none; never inferred from an earlier
    /// snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit: Option<ActionHit>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct ActionSelectedTextRange {
    pub location: u64,
    pub length: u64,
    pub units: String,
}

/// A call an agent can make next: `{tool, arguments}`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct ActionNextCall {
    pub tool: String,
    /// The call's arguments. A JSON object on the wire; the SDK records carry
    /// the same object as JSON text.
    #[serde(with = "json_object_text")]
    #[schemars(schema_with = "object_schema")]
    pub arguments: String,
}

/// Serde adapter: a JSON object on the wire, JSON text in the record.
mod json_object_text {
    use serde::{
        de::Error as _, ser::Error as _, Deserialize, Deserializer, Serialize, Serializer,
    };
    use serde_json::Value;

    pub fn serialize<S: Serializer>(text: &str, serializer: S) -> Result<S::Ok, S::Error> {
        match serde_json::from_str::<Value>(text).map_err(S::Error::custom)? {
            value @ Value::Object(_) => value.serialize(serializer),
            _ => Err(S::Error::custom(
                "next_call arguments must be a JSON object",
            )),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
        match Value::deserialize(deserializer)? {
            value @ Value::Object(_) => Ok(value.to_string()),
            _ => Err(D::Error::custom(
                "next_call arguments must be a JSON object",
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionResultValidationError {
    ConfirmedRequiresEvidence,
    PartialRequiresDeliveredCount,
    RefusedCannotHaveDelivery,
    RefusedCannotHaveEvidence,
    ErrorRequiresRefused,
    VerifiedEffectMismatch,
    ReadbackEffectMismatch,
    PointNotFinite,
    FocusRestorationReasonRequiresAbandoned,
    OpenedSurfacesRequireList,
    OpenedSurfacesForbidden,
    OpenedSurfacesContradictEffect,
    OpenedSurfaceInvalid,
}

impl std::fmt::Display for ActionResultValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::ConfirmedRequiresEvidence => "confirmed effect requires evidence",
            Self::PartialRequiresDeliveredCount => "partial effect requires delivered_count",
            Self::RefusedCannotHaveDelivery => "refused effect cannot include delivery",
            Self::RefusedCannotHaveEvidence => "refused effect cannot include evidence",
            Self::ErrorRequiresRefused => "error is only reported with a refused effect",
            Self::VerifiedEffectMismatch => "verified must agree with the confirmed effect",
            Self::ReadbackEffectMismatch => {
                "selection_changed/menu_opened true requires a confirmed effect"
            }
            Self::PointNotFinite => "point coordinates must be finite",
            Self::FocusRestorationReasonRequiresAbandoned => {
                "focus restoration reason requires abandoned status"
            }
            Self::OpenedSurfacesRequireList => {
                "observed requires a list and partial requires a non-empty list"
            }
            Self::OpenedSurfacesForbidden => {
                "absent, unavailable, or not checked status cannot have a list"
            }
            Self::OpenedSurfacesContradictEffect => "opened surfaces contradict the action effect",
            Self::OpenedSurfaceInvalid => "opened surface has invalid identity, target, or bounds",
        })
    }
}

impl std::error::Error for ActionResultValidationError {}

/// Validate the shared surface observation contract for records and wire results.
pub fn validate_opened_surfaces(
    effect: ActionEffect,
    status: Option<OpenedSurfacesStatus>,
    surfaces: Option<&[OpenedSurface]>,
) -> Result<(), ActionResultValidationError> {
    use ActionResultValidationError as Error;
    use OpenedSurfacesStatus as Status;
    match status {
        Some(Status::Observed) if surfaces.is_none() => {
            return Err(Error::OpenedSurfacesRequireList)
        }
        Some(Status::Partial) if surfaces.is_none_or(|s| s.is_empty()) => {
            return Err(Error::OpenedSurfacesRequireList)
        }
        None | Some(Status::Unavailable | Status::NotChecked) if surfaces.is_some() => {
            return Err(Error::OpenedSurfacesForbidden)
        }
        _ => {}
    }
    if (effect == ActionEffect::Refused && !matches!(status, None | Some(Status::NotChecked)))
        || (effect == ActionEffect::SuspectedNoop && surfaces.is_some_and(|s| !s.is_empty()))
    {
        return Err(Error::OpenedSurfacesContradictEffect);
    }
    for surface in surfaces.unwrap_or_default() {
        if surface.owner_pid == Some(0)
            || surface.window_id == Some(0)
            || surface.target.as_ref().is_some_and(|t| {
                t.pid == 0
                    || t.window_id == 0
                    || surface.owner_pid.is_some_and(|pid| pid != t.pid)
                    || surface.window_id.is_some_and(|id| id != t.window_id)
            })
            || surface.bounds.as_ref().is_some_and(|b| {
                !b.x.is_finite()
                    || !b.y.is_finite()
                    || !b.width.is_finite()
                    || !b.height.is_finite()
                    || b.width < 0.0
                    || b.height < 0.0
            })
        {
            return Err(Error::OpenedSurfaceInvalid);
        }
    }
    Ok(())
}

impl ActionResult {
    pub fn validate_invariants(&self) -> Result<(), ActionResultValidationError> {
        validate_opened_surfaces(
            self.effect,
            self.opened_surfaces_status,
            self.opened_surfaces.as_deref(),
        )?;
        if self
            .point
            .as_ref()
            .is_some_and(|point| !point.x.is_finite() || !point.y.is_finite())
        {
            return Err(ActionResultValidationError::PointNotFinite);
        }
        if self.focus_restoration.as_ref().is_some_and(|focus| {
            focus.status != ActionFocusRestorationStatus::Abandoned && focus.reason.is_some()
        }) {
            return Err(ActionResultValidationError::FocusRestorationReasonRequiresAbandoned);
        }
        match self.effect {
            ActionEffect::Confirmed
                if self
                    .evidence
                    .as_ref()
                    .is_none_or(|evidence| evidence.is_empty()) =>
            {
                Err(ActionResultValidationError::ConfirmedRequiresEvidence)
            }
            ActionEffect::Partial
                if self
                    .delivery
                    .as_ref()
                    .and_then(|delivery| delivery.delivered_count)
                    .is_none() =>
            {
                Err(ActionResultValidationError::PartialRequiresDeliveredCount)
            }
            ActionEffect::Refused if self.delivery.is_some() => {
                Err(ActionResultValidationError::RefusedCannotHaveDelivery)
            }
            ActionEffect::Refused if self.evidence.is_some() => {
                Err(ActionResultValidationError::RefusedCannotHaveEvidence)
            }
            effect if effect != ActionEffect::Refused && self.error.is_some() => {
                Err(ActionResultValidationError::ErrorRequiresRefused)
            }
            effect
                if self
                    .verified
                    .is_some_and(|verified| verified != (effect == ActionEffect::Confirmed)) =>
            {
                Err(ActionResultValidationError::VerifiedEffectMismatch)
            }
            ActionEffect::Confirmed
                if self.selection_changed == Some(true) || self.menu_opened == Some(true) =>
            {
                Ok(())
            }
            _ if self.selection_changed == Some(true) || self.menu_opened == Some(true) => {
                Err(ActionResultValidationError::ReadbackEffectMismatch)
            }
            _ => Ok(()),
        }
    }
}

impl ToolOutput for ActionResult {
    fn validate(&self) -> Result<(), String> {
        self.validate_invariants()
            .map_err(|error| error.to_string())
    }

    fn output_schema() -> Value {
        output_schema_with_additional_properties::<Self>(false)
    }
}

fn object_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": "object" })
}

fn string_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": "string" })
}

fn boolean_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": "boolean" })
}

fn number_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": "number" })
}

fn integer_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": "integer" })
}

fn platform_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "string",
        "enum": ["macos", "linux", "windows"]
    })
}

fn nullable_string_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "anyOf": [{ "type": "string" }, { "type": "null" }] })
}

fn nullable_cursor_point_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let point = generator.subschema_for::<CursorPointOutput>();
    schemars::json_schema!({ "anyOf": [point, { "type": "null" }] })
}

fn nullable_escalation_reason_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "anyOf": [
            {
                "type": "string",
                "enum": [
                    "ax_tree_pixel_mismatch",
                    "background_delivery_failed",
                    "foreground_ineffective",
                    "no_window_target",
                    "other"
                ]
            },
            { "type": "null" }
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn object_variant(schema: &Value) -> &Value {
        if schema.get("properties").is_some() {
            return schema;
        }
        schema["anyOf"]
            .as_array()
            .expect("nullable anyOf")
            .iter()
            .find(|variant| variant.get("properties").is_some())
            .expect("object variant")
    }

    fn confirmed_result() -> ActionResult {
        ActionResult {
            effect: ActionEffect::Confirmed,
            route: ActionRoute::Accessibility,
            delivery: Some(ActionDelivery {
                mode: ActionDeliveryMode::Background,
                delivered_count: None,
            }),
            evidence: Some(vec![ActionEvidence {
                kind: ActionEvidenceKind::ValueReadback,
                detail: None,
            }]),
            escalation: None,
            summary: None,
            note: None,
            warning: None,
            focus_restoration: None,
            error: None,
            hint: None,
            next_call: None,
            verified: None,
            selection_changed: None,
            selected_text_range: None,
            selected_text: None,
            menu_opened: None,
            menu_items: None,
            opened_surfaces: None,
            opened_surfaces_status: None,
            point: None,
            hit: None,
        }
    }

    #[test]
    fn opened_surfaces_observation_contract() {
        let base = serde_json::to_value(confirmed_result()).unwrap();
        let surface = json!({"kind":"menu", "owner_pid":7, "window_id":9,
            "target":{"pid":7,"window_id":9},
            "bounds":{"x":1.0,"y":2.0,"width":3.0,"height":4.0}, "title":"Menu"});
        for (status, list, valid) in [
            (Some("observed"), Some(vec![surface.clone()]), true),
            (Some("observed"), Some(vec![]), true),
            (Some("partial"), Some(vec![]), false),
            (Some("partial"), Some(vec![surface.clone()]), true),
            (Some("observed"), None, false),
            (Some("partial"), None, false),
            (Some("unavailable"), None, true),
            (Some("unavailable"), Some(vec![]), false),
            (Some("not_checked"), None, true),
            (Some("not_checked"), Some(vec![]), false),
            (Some("unavailable"), Some(vec![surface.clone()]), false),
            (Some("not_checked"), Some(vec![surface.clone()]), false),
            (None, None, true),
            (None, Some(vec![surface.clone()]), false),
        ] {
            let mut value = base.clone();
            if let Some(status) = status {
                value["opened_surfaces_status"] = json!(status);
            }
            if let Some(list) = list {
                value["opened_surfaces"] = json!(list);
            }
            let result: ActionResult = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(result.validate_invariants().is_ok(), valid, "{value}");
            assert_eq!(serde_json::to_value(result).unwrap(), value);
        }
    }

    #[test]
    fn opened_surface_kinds_and_embedded_targets_round_trip() {
        for kind in ["menu", "popover", "alert", "sheet", "unknown"] {
            let surface: OpenedSurface = serde_json::from_value(json!({"kind": kind})).unwrap();
            assert!(surface.target.is_none());
            assert_eq!(
                serde_json::to_value(surface).unwrap(),
                json!({"kind": kind})
            );
        }
        assert!(serde_json::from_value::<OpenedSurface>(json!({"kind":"dialog"})).is_err());
        assert!(serde_json::from_value::<OpenedSurface>(
            json!({"kind":"menu", "target":{"pid":7}})
        )
        .is_err());
    }

    #[test]
    fn opened_surfaces_reject_contradictions_and_invalid_details() {
        let base = serde_json::to_value(confirmed_result()).unwrap();
        let surface = json!({"kind":"menu", "owner_pid":7, "window_id":9,
            "target":{"pid":7,"window_id":9},
            "bounds":{"x":1.0,"y":2.0,"width":3.0,"height":4.0}});
        let mut value = base.clone();
        value["opened_surfaces_status"] = json!("partial");
        value["opened_surfaces"] = json!([surface.clone()]);
        for (path, invalid) in [
            ("/opened_surfaces/0/owner_pid", json!(0)),
            ("/opened_surfaces/0/window_id", json!(0)),
            ("/opened_surfaces/0/target/pid", json!(8)),
            ("/opened_surfaces/0/target/window_id", json!(10)),
            ("/opened_surfaces/0/target/pid", json!(0)),
            ("/opened_surfaces/0/target/window_id", json!(0)),
            ("/opened_surfaces/0/bounds/width", json!(-1.0)),
            ("/opened_surfaces/0/bounds/height", json!(-1.0)),
        ] {
            let mut invalid_value = value.clone();
            *invalid_value.pointer_mut(path).unwrap() = invalid;
            let result: ActionResult = serde_json::from_value(invalid_value).unwrap();
            assert!(result.validate_invariants().is_err(), "{path}");
        }
        let mut refused = value.clone();
        refused["effect"] = json!("refused");
        refused.as_object_mut().unwrap().remove("delivery");
        refused.as_object_mut().unwrap().remove("evidence");
        assert!(serde_json::from_value::<ActionResult>(refused.clone())
            .unwrap()
            .validate_invariants()
            .is_err());
        refused["opened_surfaces_status"] = json!("observed");
        refused["opened_surfaces"] = json!([]);
        assert!(serde_json::from_value::<ActionResult>(refused.clone())
            .unwrap()
            .validate_invariants()
            .is_err());
        refused["opened_surfaces_status"] = json!("not_checked");
        refused.as_object_mut().unwrap().remove("opened_surfaces");
        assert!(serde_json::from_value::<ActionResult>(refused.clone())
            .unwrap()
            .validate_invariants()
            .is_ok());
        refused
            .as_object_mut()
            .unwrap()
            .remove("opened_surfaces_status");
        assert!(serde_json::from_value::<ActionResult>(refused)
            .unwrap()
            .validate_invariants()
            .is_ok());
        let mut noop = value;
        noop["effect"] = json!("suspected_noop");
        assert!(serde_json::from_value::<ActionResult>(noop)
            .unwrap()
            .validate_invariants()
            .is_err());
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            for coordinate in 0..4 {
                let mut result: ActionResult = serde_json::from_value(base.clone()).unwrap();
                result.opened_surfaces_status = Some(OpenedSurfacesStatus::Partial);
                let mut item: OpenedSurface = serde_json::from_value(surface.clone()).unwrap();
                let bounds = item.bounds.as_mut().unwrap();
                *match coordinate {
                    0 => &mut bounds.x,
                    1 => &mut bounds.y,
                    2 => &mut bounds.width,
                    _ => &mut bounds.height,
                } = bad;
                result.opened_surfaces = Some(vec![item]);
                assert!(result.validate_invariants().is_err());
            }
        }
    }

    #[test]
    fn opened_surface_schema_is_closed_and_status_is_plain_enum() {
        let schema = ActionResult::output_schema();
        let status = &schema["properties"]["opened_surfaces_status"];
        assert_eq!(status["type"], "string", "{status}");
        assert_eq!(
            status["enum"],
            json!(["observed", "partial", "unavailable", "not_checked"])
        );
        let list = object_variant_or_array(&schema["properties"]["opened_surfaces"]);
        let surface = &list["items"];
        assert_eq!(surface["additionalProperties"], false);
        assert_eq!(surface["properties"]["owner_pid"]["minimum"], 1);
        assert_eq!(surface["properties"]["window_id"]["minimum"], 1);
        let target = object_variant(&surface["properties"]["target"]);
        assert_eq!(target["additionalProperties"], false);
        assert_eq!(target["properties"]["pid"]["minimum"], 1);
        assert_eq!(target["properties"]["window_id"]["minimum"], 1);
        let bounds = object_variant(&surface["properties"]["bounds"]);
        assert_eq!(bounds["additionalProperties"], false);
    }

    fn object_variant_or_array(schema: &Value) -> &Value {
        if schema.get("items").is_some() {
            return schema;
        }
        schema["anyOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v.get("items").is_some())
            .unwrap()
    }

    #[test]
    fn action_disclosures_round_trip_and_validate_restoration_reason() {
        for focus in [
            serde_json::json!({"status":"restored", "prior_pid":1}),
            serde_json::json!({"status":"abandoned", "reason":"third_app", "prior_pid":1}),
            serde_json::json!({"status":"abandoned", "prior_pid":null}),
        ] {
            let mut value = serde_json::to_value(confirmed_result()).unwrap();
            value["note"] = "note".into();
            value["warning"] = "warning".into();
            value["focus_restoration"] = focus;
            let result: ActionResult = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(result.validate_invariants(), Ok(()));
            assert_eq!(serde_json::to_value(result).unwrap(), value);
        }
        let mut result = confirmed_result();
        result.focus_restoration = Some(ActionFocusRestoration {
            status: ActionFocusRestorationStatus::Restored,
            reason: Some(ActionFocusRestorationReason::ThirdApp),
            prior_pid: Some(1),
        });
        assert_eq!(
            result.validate_invariants(),
            Err(ActionResultValidationError::FocusRestorationReasonRequiresAbandoned)
        );
    }

    #[test]
    fn schema_compaction_keeps_properties_named_title_or_description() {
        let mut schema = serde_json::json!({
            "title": "TopLevel",
            "description": "Top-level annotation",
            "properties": {
                "title": { "type": "string", "description": "The window title." },
                "description": { "type": "string" },
                "nested": {
                    "title": "Nested",
                    "properties": { "title": { "type": "string", "title": "Inner" } }
                }
            }
        });

        strip_schema_titles(&mut schema);

        assert!(schema.get("title").is_none(), "annotation must be stripped");
        assert!(
            schema.get("description").is_none(),
            "annotation must be stripped"
        );
        let properties = schema["properties"].as_object().expect("properties map");
        assert!(
            properties.contains_key("title"),
            "property name must survive compaction"
        );
        assert!(
            properties.contains_key("description"),
            "property name must survive compaction"
        );
        assert!(
            properties["nested"]["properties"]
                .as_object()
                .expect("nested properties")
                .contains_key("title"),
            "nested property name must survive compaction"
        );
        assert!(properties["title"].get("description").is_none());
        assert!(properties["nested"].get("title").is_none());
    }

    #[test]
    fn action_result_schema_is_exact_and_closed() {
        let schema = ActionResult::output_schema();
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], json!(["effect", "route"]));

        let properties = schema["properties"].as_object().expect("properties");
        assert_eq!(
            properties.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "delivery",
                "effect",
                "error",
                "escalation",
                "evidence",
                "focus_restoration",
                "hint",
                "hit",
                "menu_items",
                "menu_opened",
                "next_call",
                "note",
                "opened_surfaces",
                "opened_surfaces_status",
                "point",
                "route",
                "selected_text",
                "selected_text_range",
                "selection_changed",
                "summary",
                "verified",
                "warning"
            ]
        );
        assert_eq!(
            properties["effect"]["enum"],
            json!([
                "confirmed",
                "partial",
                "unverifiable",
                "suspected_noop",
                "refused"
            ])
        );
        assert_eq!(
            properties["route"]["enum"],
            json!([
                "accessibility",
                "synthetic_events",
                "global_input",
                "system_api",
                "dom",
                "trusted_input"
            ])
        );

        let delivery = object_variant(&properties["delivery"]);
        assert_eq!(delivery["additionalProperties"], false);
        assert_eq!(delivery["required"], json!(["mode"]));
        assert_eq!(
            delivery["properties"]["mode"]["enum"],
            json!(["background", "foreground", "not_applicable", "unknown"])
        );

        let evidence_schema = &properties["evidence"];
        let evidence_array = if evidence_schema.get("items").is_some() {
            evidence_schema
        } else {
            evidence_schema["anyOf"]
                .as_array()
                .expect("nullable anyOf")
                .iter()
                .find(|variant| variant.get("items").is_some())
                .expect("array variant")
        };
        let evidence = &evidence_array["items"];
        assert_eq!(evidence["additionalProperties"], false);
        assert_eq!(evidence["required"], json!(["kind"]));
        assert_eq!(
            evidence["properties"]["kind"]["enum"],
            json!(["value_readback", "window_change"])
        );

        let escalation = object_variant(&properties["escalation"]);
        assert_eq!(escalation["additionalProperties"], false);
        assert_eq!(escalation["required"], json!(["target", "reason"]));
        assert_eq!(
            escalation["properties"]["target"]["enum"],
            json!(["pixel", "foreground", "page", "session"])
        );
        assert_eq!(
            escalation["properties"]["reason"]["enum"],
            json!([
                "route_unavailable",
                "delivery_failed",
                "effect_unconfirmed",
                "suspected_noop",
                "permission_required"
            ])
        );
    }

    #[test]
    fn action_result_round_trips_without_legacy_or_request_fields() {
        let result = confirmed_result();
        let value = serde_json::to_value(&result).expect("serialize");
        assert_eq!(
            value,
            json!({
                "effect": "confirmed",
                "route": "accessibility",
                "delivery": {"mode": "background"},
                "evidence": [{"kind": "value_readback"}]
            })
        );
        assert_eq!(
            serde_json::from_value::<ActionResult>(value).expect("deserialize"),
            result
        );

        for legacy in [
            ("scope", json!("desktop")),
            ("target", json!("button")),
            ("x", json!(10)),
            ("y", json!(20)),
            ("path", json!("cgevent")),
            ("transport", json!("windows_send_input")),
            ("extensions", json!({})),
        ] {
            let mut value = serde_json::to_value(&result).expect("serialize");
            value
                .as_object_mut()
                .expect("object")
                .insert(legacy.0.into(), legacy.1);
            assert!(
                serde_json::from_value::<ActionResult>(value).is_err(),
                "accepted legacy field {}",
                legacy.0
            );
        }

        let mut delivery_extension = serde_json::to_value(&result).expect("serialize");
        delivery_extension["delivery"]["requested"] = json!("background");
        assert!(serde_json::from_value::<ActionResult>(delivery_extension).is_err());

        // Evidence carries its human detail so a client that reads only
        // structuredContent still sees what the readback said; other
        // evidence extensions stay closed.
        let mut evidence_detail = serde_json::to_value(&result).expect("serialize");
        evidence_detail["evidence"][0]["detail"] = json!("value read back as 42");
        assert_eq!(
            serde_json::from_value::<ActionResult>(evidence_detail)
                .expect("detail")
                .evidence
                .expect("evidence")[0]
                .detail
                .as_deref(),
            Some("value read back as 42")
        );
        let mut evidence_extension = serde_json::to_value(&result).expect("serialize");
        evidence_extension["evidence"][0]["raw"] = json!("private readback");
        assert!(serde_json::from_value::<ActionResult>(evidence_extension).is_err());

        let refusal = json!({
            "effect": "refused",
            "route": "synthetic_events",
            "summary": "no input was sent",
            "error": {"code": "point_outside_window", "hint": "use window-local pixels"}
        });
        let refusal = serde_json::from_value::<ActionResult>(refusal).expect("refusal");
        assert_eq!(refusal.validate_invariants(), Ok(()));
        assert_eq!(
            refusal.error.as_ref().map(|error| error.code.as_str()),
            Some("point_outside_window")
        );
        let mut error_extension = serde_json::to_value(&refusal).expect("serialize");
        error_extension["error"]["detail"] = json!({"x": 1});
        assert!(serde_json::from_value::<ActionResult>(error_extension).is_err());

        let escalation_extension = json!({
            "effect": "unverifiable",
            "route": "synthetic_events",
            "escalation": {
                "target": "foreground",
                "reason": "delivery_failed",
                "requires": ["window_id"]
            }
        });
        assert!(serde_json::from_value::<ActionResult>(escalation_extension).is_err());
    }

    #[test]
    fn wave400c_action_result_accepts_optional_selection_and_menu_readback() {
        let value = json!({
            "effect": "confirmed",
            "route": "accessibility",
            "evidence": [{"kind": "value_readback"}],
            "verified": true,
            "selection_changed": true,
            "selected_text_range": {"location": 2, "length": 4, "units": "utf16"},
            "selected_text": "text",
            "menu_opened": true,
            "menu_items": ["Copy", "Paste"]
        });
        let result = serde_json::from_value::<ActionResult>(value.clone())
            .expect("Wave 400-C readback fields are part of the additive action contract");
        assert_eq!(serde_json::to_value(result).unwrap(), value);

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct LegacyStrictActionResult {
            #[allow(dead_code)]
            effect: ActionEffect,
            #[allow(dead_code)]
            route: ActionRoute,
        }
        assert!(
            serde_json::from_value::<LegacyStrictActionResult>(value).is_err(),
            "documented compatibility limit: old strict clients must tolerate or learn the additive fields"
        );
    }

    #[test]
    fn pointer_point_and_hit_round_trip_as_closed_nested_records() {
        let wire = json!({
            "effect": "unverifiable",
            "route": "synthetic_events",
            "point": {"x": 80.0, "y": 75.5, "coordinate_space": "window_points"},
            "hit": {"index": 3, "role": "AXButton", "label": "OK", "identifier": "ok"}
        });
        let result = serde_json::from_value::<ActionResult>(wire.clone()).expect("point and hit");
        assert_eq!(result.validate_invariants(), Ok(()));
        let point = result.point.as_ref().expect("point");
        assert_eq!((point.x, point.y), (80.0, 75.5));
        assert_eq!(result.hit.as_ref().and_then(|hit| hit.index), Some(3));
        assert_eq!(serde_json::to_value(&result).expect("serialize"), wire);

        let mut extra = wire.clone();
        extra["point"]["unit"] = json!("window_points");
        assert!(serde_json::from_value::<ActionResult>(extra).is_err());
        let mut extra = wire;
        extra["hit"]["name"] = json!("OK");
        assert!(serde_json::from_value::<ActionResult>(extra).is_err());

        let schema = ActionResult::output_schema();
        let point = object_variant(&schema["properties"]["point"]);
        assert_eq!(point["additionalProperties"], false);
        assert_eq!(point["required"], json!(["x", "y"]));
        assert_eq!(point["properties"]["x"]["type"], "number");
        let hit = object_variant(&schema["properties"]["hit"]);
        assert_eq!(hit["additionalProperties"], false);
        assert!(hit
            .get("required")
            .is_none_or(|required| required == &json!([])));
        for name in ["click", "drag", "scroll"] {
            let advertised = crate::advertised_tool_output_schema(name).expect("action schema");
            // The success arm a proxy compares against its daemon's schema.
            let success = &advertised["anyOf"][0]["properties"];
            assert!(success.get("point").is_some(), "{name}");
            assert!(success.get("hit").is_some(), "{name}");
        }
    }

    #[test]
    fn pointer_point_and_hit_are_additive_with_the_documented_strict_client_limit() {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct LegacyStrictActionResult {
            #[allow(dead_code)]
            effect: ActionEffect,
            #[allow(dead_code)]
            route: ActionRoute,
        }
        let base = json!({"effect": "unverifiable", "route": "synthetic_events"});
        assert!(
            serde_json::from_value::<LegacyStrictActionResult>(base.clone()).is_ok(),
            "the fixture is a payload an old strict client accepts"
        );
        for (key, value) in [
            (
                "point",
                json!({"x": 1.0, "y": 2.0, "coordinate_space": "window_points"}),
            ),
            ("hit", json!({"role": "push button", "label": "OK"})),
        ] {
            let mut extended = base.clone();
            extended[key] = value;
            assert!(
                serde_json::from_value::<LegacyStrictActionResult>(extended.clone()).is_err(),
                "documented compatibility limit: an old strict client rejects {key}"
            );
            let current = serde_json::from_value::<ActionResult>(extended.clone())
                .unwrap_or_else(|error| panic!("{key} is part of the contract: {error}"));
            assert_eq!(serde_json::to_value(current).unwrap(), extended);
        }
    }

    #[test]
    fn action_result_rejects_a_non_finite_point() {
        let mut result = confirmed_result();
        result.point = Some(ActionPoint {
            x: f64::NAN,
            y: 1.0,
            coordinate_space: None,
        });
        assert_eq!(
            result.validate_invariants(),
            Err(ActionResultValidationError::PointNotFinite)
        );
        result.point = Some(ActionPoint {
            x: 1.0,
            y: f64::INFINITY,
            coordinate_space: None,
        });
        assert_eq!(
            result.validate_invariants(),
            Err(ActionResultValidationError::PointNotFinite)
        );
        result.point = Some(ActionPoint {
            x: -4.0,
            y: 1.0,
            coordinate_space: Some("window_points".into()),
        });
        assert_eq!(result.validate_invariants(), Ok(()));
    }

    #[test]
    fn hint_and_next_call_travel_as_a_string_and_a_json_object() {
        let wire = json!({
            "effect": "unverifiable",
            "route": "synthetic_events",
            "hint": "Read what changed before clicking again.",
            "next_call": {
                "tool": "get_window_state",
                "arguments": {"pid": 7, "window_id": 3, "since": "latest"}
            }
        });
        let result = serde_json::from_value::<ActionResult>(wire.clone()).expect("hint");
        assert_eq!(result.validate_invariants(), Ok(()));
        assert_eq!(
            result.hint.as_deref(),
            Some("Read what changed before clicking again.")
        );
        let call = result.next_call.as_ref().expect("next_call");
        assert_eq!(call.tool, "get_window_state");
        // The record keeps the arguments as JSON text, so every SDK language reads them.
        assert_eq!(
            serde_json::from_str::<Value>(&call.arguments).expect("json text"),
            json!({"pid": 7, "window_id": 3, "since": "latest"})
        );
        // The wire form is the object again.
        assert_eq!(serde_json::to_value(&result).expect("serialize"), wire);

        let mut not_an_object = wire.clone();
        not_an_object["next_call"]["arguments"] = json!("get_window_state");
        assert!(serde_json::from_value::<ActionResult>(not_an_object).is_err());
        let mut extra = wire;
        extra["next_call"]["note"] = json!("x");
        assert!(serde_json::from_value::<ActionResult>(extra).is_err());

        let schema = ActionResult::output_schema();
        let next_call = object_variant(&schema["properties"]["next_call"]);
        assert_eq!(next_call["additionalProperties"], false);
        assert_eq!(next_call["required"], json!(["tool", "arguments"]));
        assert_eq!(next_call["properties"]["arguments"]["type"], "object");
    }

    #[test]
    fn action_result_enforces_effect_invariants() {
        let mut result = confirmed_result();
        result.evidence = None;
        assert_eq!(
            result.validate_invariants(),
            Err(ActionResultValidationError::ConfirmedRequiresEvidence)
        );
        assert_eq!(
            ToolOutput::validate(&result),
            Err("confirmed effect requires evidence".into())
        );

        result.effect = ActionEffect::Partial;
        result.delivery = Some(ActionDelivery {
            mode: ActionDeliveryMode::Foreground,
            delivered_count: None,
        });
        assert_eq!(
            result.validate_invariants(),
            Err(ActionResultValidationError::PartialRequiresDeliveredCount)
        );
        result.delivery.as_mut().expect("delivery").delivered_count = Some(1);
        assert_eq!(result.validate_invariants(), Ok(()));

        result.effect = ActionEffect::Refused;
        assert_eq!(
            result.validate_invariants(),
            Err(ActionResultValidationError::RefusedCannotHaveDelivery)
        );
        result.delivery = None;
        result.evidence = Some(vec![ActionEvidence {
            kind: ActionEvidenceKind::WindowChange,
            detail: None,
        }]);
        assert_eq!(
            result.validate_invariants(),
            Err(ActionResultValidationError::RefusedCannotHaveEvidence)
        );
        result.evidence = None;
        assert_eq!(result.validate_invariants(), Ok(()));

        result.error = Some(ActionError {
            code: "window_target_not_found".into(),
            hint: None,
        });
        assert_eq!(result.validate_invariants(), Ok(()));
        result.effect = ActionEffect::Unverifiable;
        assert_eq!(
            result.validate_invariants(),
            Err(ActionResultValidationError::ErrorRequiresRefused)
        );
    }
}

#[cfg(test)]
mod error_envelope_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_diagnostic_without_a_marker_gains_one() {
        let envelope = conforming_error_envelope(json!({"exit_code": 1}));

        assert_eq!(envelope["code"], TOOL_INVOCATION_FAILED_CODE);
        assert_eq!(envelope["exit_code"], 1);
    }

    #[test]
    fn each_existing_marker_is_left_alone() {
        for marker in ["refusal", "status", "code"] {
            let envelope = conforming_error_envelope(json!({marker: "already-named"}));

            assert_eq!(
                envelope,
                json!({marker: "already-named"}),
                "guard rewrote a payload that already carries `{marker}`"
            );
        }
    }

    #[test]
    fn a_non_object_diagnostic_becomes_an_object() {
        let envelope = conforming_error_envelope(json!("daemon transport closed"));

        assert_eq!(envelope["code"], TOOL_INVOCATION_FAILED_CODE);
        assert_eq!(envelope["detail"], "daemon transport closed");
    }

    #[test]
    fn an_absent_diagnostic_still_produces_a_refusal() {
        assert_eq!(
            conforming_error_envelope(Value::Null),
            json!({"code": TOOL_INVOCATION_FAILED_CODE})
        );
    }
}
