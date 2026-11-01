// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! Portable native application discovery and exact-window observations.

use crate::{Role, State, ToolInput, ToolOutput};
use schemars::{json_schema, JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize};

fn string_schema(g: &mut SchemaGenerator) -> Schema {
    String::json_schema(g)
}
fn bool_schema(g: &mut SchemaGenerator) -> Schema {
    bool::json_schema(g)
}
fn pid_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"integer", "minimum":1, "maximum":4294967295_u64})
}
fn positive_integer_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"integer", "minimum":1})
}
fn nonnegative_integer_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"integer", "minimum":0})
}

fn nullable_pid_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":["integer","null"], "minimum":0, "maximum":4294967295_u64})
}

fn nullable_z_index_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":["integer","null"]})
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct ListAppsInput {}

impl ToolInput for ListAppsInput {
    const TOOL_NAME: &'static str = "list_apps";
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct ListWindowsInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "pid_schema")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "bool_schema")]
    pub on_screen_only: Option<bool>,
}

impl ToolInput for ListWindowsInput {
    const TOOL_NAME: &'static str = "list_windows";

    fn validate(&self) -> Result<(), String> {
        if self.pid == Some(0) {
            return Err("window discovery requires a positive process ID".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct GetWindowStateInput {
    #[schemars(schema_with = "pid_schema")]
    pub pid: u32,
    #[schemars(schema_with = "positive_integer_schema")]
    pub window_id: u64,
    /// Expand only the subtree rooted at this snapshot-qualified element token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "string_schema")]
    #[uniffi(default = None)]
    pub root_element_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "string_schema")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "string_schema")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "bool_schema")]
    pub include_accessibility_tree: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "bool_schema")]
    pub include_screenshot: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "string_schema")]
    pub screenshot_out_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "positive_integer_schema")]
    pub max_elements: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "positive_integer_schema")]
    pub max_depth: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "positive_integer_schema")]
    pub max_dimension: Option<u32>,
    /// Optional per-call long-edge ceiling. Zero requests native resolution;
    /// omit it to preserve the configured session or global behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "nonnegative_integer_schema")]
    pub max_image_dimension: Option<u32>,
    /// Wall-clock budget for the accessibility walk in milliseconds
    /// (default 1000 on every platform). A walk that runs out returns a
    /// partial tree flagged `truncated` rather than failing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "timeout_ms_schema")]
    #[uniffi(default = None)]
    pub timeout_ms: Option<u32>,
    /// Tree representation to return: `markdown` (default), `elements`, or
    /// `both`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "tree_format_schema")]
    #[uniffi(default = None)]
    pub tree_format: Option<String>,
    /// Return only what changed since this earlier `snapshot_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "string_schema")]
    #[uniffi(default = None)]
    pub since: Option<String>,
    /// Include `_note` and the full `background_input` report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "bool_schema")]
    #[uniffi(default = None)]
    pub verbose: Option<bool>,
    /// Restore the previous full response (both representations, all
    /// metadata, platform walk limits).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "bool_schema")]
    #[uniffi(default = None)]
    pub full_output: Option<bool>,
}

fn tree_format_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type":"string", "enum":["markdown","elements","both"]})
}

/// Bounds of the accessibility-walk budget, shared with every live backend
/// schema (`cua_driver_core::tool_schema::timeout_ms_schema`).
pub const TIMEOUT_MS_MIN: u32 = 100;
pub const TIMEOUT_MS_MAX: u32 = 120_000;

fn timeout_ms_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "integer",
        "minimum": TIMEOUT_MS_MIN,
        "maximum": TIMEOUT_MS_MAX
    })
}

impl ToolInput for GetWindowStateInput {
    const TOOL_NAME: &'static str = "get_window_state";
    fn validate(&self) -> Result<(), String> {
        if self.pid == 0 || self.window_id == 0 {
            return Err("window observation requires positive process and window IDs".into());
        }
        if self.root_element_token.is_some() {
            if self.since.is_some() {
                return Err("root_since_unsupported".into());
            }
            if self.include_accessibility_tree == Some(false) {
                return Err("root requires an accessibility tree".into());
            }
        }
        if [self.max_elements, self.max_depth, self.max_dimension].contains(&Some(0)) {
            return Err("window observation limits must be positive".into());
        }
        if self
            .timeout_ms
            .is_some_and(|ms| !(TIMEOUT_MS_MIN..=TIMEOUT_MS_MAX).contains(&ms))
        {
            return Err(format!(
                "timeout_ms must be between {TIMEOUT_MS_MIN} and {TIMEOUT_MS_MAX} milliseconds"
            ));
        }
        if self.include_accessibility_tree == Some(false) && self.include_screenshot == Some(false)
        {
            return Err("window observation requires accessibility or screenshot capture".into());
        }
        Ok(())
    }
}

fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
pub struct AppInfo {
    pub pid: u32,
    pub name: String,
    pub running: bool,
    pub active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
pub struct ListAppsOutput {
    pub apps: Vec<AppInfo>,
}

impl ToolOutput for ListAppsOutput {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
pub struct WindowBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
pub struct WindowInfo {
    pub window_id: u64,
    #[serde(deserialize_with = "required_nullable")]
    #[schemars(required, schema_with = "nullable_pid_schema")]
    pub pid: Option<u32>,
    pub app_name: String,
    pub title: String,
    pub bounds: WindowBounds,
    pub is_on_screen: bool,
    #[serde(deserialize_with = "required_nullable")]
    #[schemars(required, schema_with = "nullable_z_index_schema")]
    /// Higher values are closer to the front. Null means stacking order is unavailable; callers must not infer an order from array position.
    pub z_index: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimized: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_space_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_current_space: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_ids: Option<Vec<u64>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
pub struct ListWindowsOutput {
    pub windows: Vec<WindowInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_space_id: Option<u64>,
}

impl ToolOutput for ListWindowsOutput {
    fn output_schema() -> serde_json::Value {
        let mut schema = crate::outputs::output_schema_with_additional_properties::<Self>(true);
        // Stacking semantics are part of the existing live discovery contract.
        schema["properties"]["windows"]["items"]["properties"]["z_index"]["description"] =
            serde_json::json!("Higher values are closer to the front. Null means the provider cannot observe stacking order; callers must not infer an order from array position or treat null as zero.");
        // Window-closure evidence. These keys are part of the wire contract
        // only, not of the record: adding them to the struct would change its
        // UniFFI layout under the checked-in bindings. The typed SDK record
        // ignores them; MCP clients read them from structuredContent.
        schema["properties"]["enumeration_complete"] = serde_json::json!({
            "type": "boolean",
            "description": "True when the window listing, and with pid the identity snapshot below, were read from the current user session's WindowServer in full. False means a missing window may only be unreadable, so its absence proves nothing."
        });
        schema["properties"]["authoritative_window_ids"] = serde_json::json!({
            "type": "array",
            "items": {"type": "integer", "minimum": 1},
            "description": "Present only with pid and enumeration_complete true: every window id WindowServer reported for that pid at snapshot time, on every layer and Space, on or off screen, before the listing dropped helper windows, the driver's overlay and app-filtered windows. Ascending. An id seen earlier for this pid and absent here was no longer a WindowServer window of that pid at snapshot time; pair it with process identity and timing, because the process may have exited."
        });
        schema
    }
}

/// The point a pointer action aimed at, as its producer reports it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct ActionPoint {
    pub x: f64,
    pub y: f64,
    /// The space `x`/`y` are in, such as `window_points`. Absent when the
    /// producer did not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinate_space: Option<String>,
}

/// The element a pointer route reported hitting when it dispatched.
#[derive(
    Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record,
)]
#[serde(deny_unknown_fields)]
pub struct ActionHit {
    /// Element index in the snapshot the route resolved against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u64>,
    /// Accessibility role, in the platform's vocabulary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Accessible name or label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Stable accessibility identifier, when the element has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
pub struct ElementFrame {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// A Windows TogglePattern state: a check box or toggle button is on, off, or
/// mixed (indeterminate, such as a parent of partly checked items).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum ToggleState {
    On,
    Off,
    Mixed,
}

/// `toggle_state` is optional by omission. Its schema is the plain string enum: an
/// `Option` schema would add `null` to `enum`, which Gemini-compatible clients reject for
/// the whole tool list (protocol_schema_test).
fn toggle_state_schema(generator: &mut SchemaGenerator) -> Schema {
    ToggleState::json_schema(generator)
}

fn role_schema(generator: &mut SchemaGenerator) -> Schema {
    Role::json_schema(generator)
}

fn states_schema(generator: &mut SchemaGenerator) -> Schema {
    Vec::<State>::json_schema(generator)
}

/// Broad cause of a missing part of the requested observation.
#[derive(
    Debug,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    JsonSchema,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    uniffi::Enum,
)]
#[serde(rename_all = "snake_case")]
pub enum IncompleteReasonKind {
    PolicyOmission,
    ProviderFailure,
    Budget,
    Unknown,
}

#[derive(
    Debug,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    JsonSchema,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    uniffi::Enum,
)]
#[serde(rename_all = "snake_case")]
pub enum IncompleteReasonDetail {
    CollapsedMenu,
    Offscreen,
    InactiveTab,
    WebContent,
    HugeContainer,
    DepthLimit,
    NodeBudget,
    TimeBudget,
    ChildEnumerationFailed,
    AttributeReadFailed,
    EnumerationErrorUnclassified,
    UntrustedProvider,
    ScopeMismatch,
    NotAsserted,
}

#[derive(
    Debug,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    JsonSchema,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    uniffi::Record,
)]
pub struct IncompleteReason {
    pub kind: IncompleteReasonKind,
    pub detail: IncompleteReasonDetail,
}

/// A boundary and a read-level omission use the same vocabulary.
pub type BoundaryReason = IncompleteReason;

/// Evidence for the number of direct children omitted at this node.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum CountEvidence {
    Known(u32),
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
pub struct NodeBoundary {
    pub reasons: Vec<IncompleteReason>,
    pub children_skipped: CountEvidence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub descendants_omitted: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
pub struct WindowElement {
    pub element_index: u64,
    pub role: String,
    /// Canonical role from successful platform evidence. Omitted if the role read
    /// failed; Unknown means the read succeeded but the evidence was unrecognized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "role_schema")]
    pub role_canonical: Option<Role>,
    /// Asserted states from the same evidence as enabled, selected, toggle_state,
    /// and expanded; those fields are authoritative. A missing token never means
    /// false. Focused is asserted only where it was read. Secure is asserted only
    /// by existing secure detection; its absence does not establish that an
    /// element is non-secure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "states_schema")]
    pub states: Option<Vec<State>>,
    pub depth: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary: Option<NodeBoundary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<bool>,
    /// TogglePattern state (Windows), or the AT-SPI CHECKED/INDETERMINATE state of a
    /// check box, radio button, toggle button or switch (Linux).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "toggle_state_schema")]
    pub toggle_state: Option<ToggleState>,
    /// ExpandCollapsePattern state (Windows; partly expanded counts as expanded), or
    /// AT-SPI EXPANDED/EXPANDABLE (Linux). Absent for elements that cannot expand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expanded: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_web_content: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_index: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<ElementFrame>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
pub struct SnapshotImage {
    pub mime_type: String,
    pub data_base64: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum DrilldownStatus {
    Complete,
    Incomplete,
    StaleParent,
    Unsupported,
}

fn drilldown_status_schema(generator: &mut SchemaGenerator) -> Schema {
    DrilldownStatus::json_schema(generator)
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
pub struct DrilldownScope {
    pub root_token: String,
    pub parent_snapshot_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
pub struct WindowStateOutput {
    pub pid: u32,
    pub window_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "drilldown_status_schema")]
    pub drilldown_status: Option<DrilldownStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<DrilldownScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drilldown_diagnostics: Option<Vec<IncompleteReason>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_markdown: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elements: Option<Vec<WindowElement>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_element_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub returned_element_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filtered_element_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elements_complete: Option<bool>,
    /// Producer assertion about the requested scope. Absence is incomplete;
    /// reductions after the walk do not change observation completeness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_complete: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_incomplete_reasons: Option<Vec<IncompleteReason>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation_reason: Option<String>,
    /// Plain-language line saying the tree was cut at `max_elements` and how
    /// to read more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation_hint: Option<String>,
    /// Which representation this response carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_format: Option<String>,
    /// The `since` snapshot_id the caller asked to diff against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// `diff`, `no_change`, or the reason a full read was returned instead
    /// (`unknown_snapshot`, `other_window`, `view_changed`, `too_much_changed`,
    /// `diff_too_large`, `no_snapshot`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_status: Option<String>,
    /// Added/changed/removed rows against `since`, one per line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_diff: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot_width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot_height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot_scale: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot_mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot_file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot_frame_valid: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_bounds: Option<WindowBounds>,
    /// Image content belongs to the MCP envelope, never structuredContent.
    #[serde(skip)]
    #[schemars(skip)]
    pub images: Vec<SnapshotImage>,
}

impl ToolOutput for WindowStateOutput {
    fn validate(&self) -> Result<(), String> {
        match self.drilldown_status {
            Some(DrilldownStatus::Complete) => {
                if self.scope.is_none()
                    || self.snapshot_id.is_none()
                    || self.observation_complete != Some(true)
                {
                    return Err(
                        "complete drill-down requires a scope, snapshot and complete observation"
                            .into(),
                    );
                }
            }
            Some(
                DrilldownStatus::Incomplete
                | DrilldownStatus::StaleParent
                | DrilldownStatus::Unsupported,
            ) => {
                if self.snapshot_id.is_some()
                    || self.tree_markdown.is_some()
                    || self.elements.is_some()
                    || self.tree_diff.is_some()
                {
                    return Err(
                        "incomplete drill-down cannot expose a snapshot or replacement rows".into(),
                    );
                }
                if self.drilldown_status == Some(DrilldownStatus::Incomplete)
                    && (self.observation_complete != Some(false)
                        || self
                            .drilldown_diagnostics
                            .as_ref()
                            .is_none_or(Vec::is_empty))
                {
                    return Err(
                        "incomplete drill-down requires an incomplete observation and diagnostics"
                            .into(),
                    );
                }
            }
            _ => {}
        }
        match (self.screenshot_width, self.screenshot_height) {
            (None, None) => {}
            (Some(width), Some(height)) if width > 0 && height > 0 => {}
            _ => return Err("screenshot dimensions must be a positive width/height pair".into()),
        }
        if self
            .screenshot_scale
            .is_some_and(|scale| !scale.is_finite() || scale <= 0.0)
        {
            return Err("screenshot_scale must be finite and positive".into());
        }
        if let (Some(elements), Some(returned)) = (&self.elements, self.returned_element_count) {
            if elements.len() as u64 != returned {
                return Err("returned_element_count does not match elements".into());
            }
        }
        if let (Some(total), Some(returned)) =
            (self.total_element_count, self.returned_element_count)
        {
            if returned > total {
                return Err("returned_element_count exceeds total_element_count".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_vocabulary_roundtrips_and_rejects_invalid_values() {
        let old = json!({"element_index":1,"role":"AXButton","depth":0});
        let old_element: WindowElement = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(serde_json::to_value(old_element).unwrap(), old);
        let new = json!({"element_index":1,"role":"AXButton","depth":0,
            "role_canonical":"button","states":["focused","disabled","haspopup"]});
        let element: WindowElement = serde_json::from_value(new.clone()).unwrap();
        assert_eq!(serde_json::to_value(element).unwrap(), new);
        let schema = WindowStateOutput::output_schema();
        let properties = &schema["properties"]["elements"]["items"]["properties"];
        assert_eq!(properties["role_canonical"]["type"], "string");
        assert!(!properties["role_canonical"]["enum"]
            .as_array()
            .unwrap()
            .contains(&serde_json::Value::Null));
        assert_eq!(properties["states"]["type"], "array");
        assert_eq!(properties["states"]["items"]["type"], "string");
        assert!(!properties["states"]["items"]["enum"]
            .as_array()
            .unwrap()
            .contains(&serde_json::Value::Null));
        for invalid in [
            json!({"role_canonical":"bogus"}),
            json!({"states":["bogus"]}),
        ] {
            let mut value = old.clone();
            value
                .as_object_mut()
                .unwrap()
                .extend(invalid.as_object().unwrap().clone());
            assert!(serde_json::from_value::<WindowElement>(value).is_err());
        }
    }

    #[test]
    fn window_observation_fields_roundtrip_and_omit_when_absent() {
        let old = json!({"pid":7,"window_id":9,"elements":[{"element_index":1,"role":"button","depth":0}]});
        let output: WindowStateOutput = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(serde_json::to_value(output).unwrap(), old);
        let new = json!({"pid":7,"window_id":9,"observation_complete":false,
            "observation_incomplete_reasons":[{"kind":"budget","detail":"node_budget"}],
            "elements":[{"element_index":1,"role":"button","depth":0,"boundary":{
                "reasons":[{"kind":"budget","detail":"node_budget"}],"children_skipped":{"known":4},"descendants_omitted":true}}]});
        let output: WindowStateOutput = serde_json::from_value(new.clone()).unwrap();
        assert_eq!(serde_json::to_value(output).unwrap(), new);
        let schema = WindowStateOutput::output_schema();
        assert!(schema["properties"]["observation_complete"].is_object());
        assert!(schema["properties"]["elements"]
            .to_string()
            .contains("boundary"));
    }

    #[test]
    fn drilldown_contract_roundtrips_and_omits_absent_fields() {
        let old = json!({"pid":7,"window_id":9});
        let input: GetWindowStateInput = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(serde_json::to_value(input).unwrap(), old);
        let scoped = json!({"pid":7,"window_id":9,"root_element_token":"s0000002a:1"});
        let input: GetWindowStateInput = serde_json::from_value(scoped.clone()).unwrap();
        assert_eq!(serde_json::to_value(input).unwrap(), scoped);
        assert!(serde_json::from_value::<GetWindowStateInput>(json!({
            "pid":7,"window_id":9,"root_element_tokenn":"s0000002a:1"
        }))
        .is_err());
        let output = json!({"pid":7,"window_id":9,"drilldown_status":"incomplete",
            "scope":{"root_token":"s0000002a:1","parent_snapshot_id":"s0000002a"},
            "drilldown_diagnostics":[{"kind":"budget","detail":"node_budget"}]});
        let decoded: WindowStateOutput = serde_json::from_value(output.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), output);
        let old_output = json!({"pid":7,"window_id":9});
        let decoded: WindowStateOutput = serde_json::from_value(old_output.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), old_output);
        let schema = WindowStateOutput::output_schema();
        assert_eq!(schema["properties"]["drilldown_status"]["type"], "string");
        assert!(!schema["properties"]["drilldown_status"]["enum"]
            .as_array()
            .unwrap()
            .contains(&serde_json::Value::Null));
    }

    #[test]
    fn drilldown_rejects_since_and_screenshot_only() {
        for extra in [
            json!({"since":"latest"}),
            json!({"include_accessibility_tree":false}),
        ] {
            let mut value = json!({"pid":7,"window_id":9,"root_element_token":"s0000002a:1"});
            value
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let input: GetWindowStateInput = serde_json::from_value(value).unwrap();
            assert!(input.validate().is_err());
        }
    }

    #[test]
    fn drilldown_outcome_refuses_actionable_incomplete_results() {
        for extra in [
            json!({"snapshot_id":"s000000bb"}),
            json!({"tree_markdown":"- [0] button"}),
            json!({"elements":[{"element_index":0,"role":"button","depth":0}]}),
        ] {
            let mut value = json!({"pid":7,"window_id":9,"drilldown_status":"incomplete",
                "drilldown_diagnostics":[{"kind":"budget","detail":"node_budget"}]});
            value
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let output: WindowStateOutput = serde_json::from_value(value).unwrap();
            assert!(output.validate().is_err());
        }
        let output: WindowStateOutput = serde_json::from_value(json!({"pid":7,"window_id":9,
            "drilldown_status":"complete","observation_complete":false,"snapshot_id":"s000000bb",
            "scope":{"root_token":"s000000aa:1","parent_snapshot_id":"s000000aa"}}))
        .unwrap();
        assert!(output.validate().is_err());
        let output: WindowStateOutput = serde_json::from_value(json!({"pid":7,"window_id":9,
            "drilldown_status":"incomplete","drilldown_diagnostics":[{"kind":"unknown","detail":"not_asserted"}]})).unwrap();
        assert!(output.validate().is_err());
    }

    #[test]
    fn native_app_success_shapes_allow_unavailable_metadata() {
        for metadata in [
            json!({"bundle_id":"org.example.App", "kind":"desktop", "launch_path":"/Applications/Example.app", "last_used":null}),
            json!({"bundle_id":null, "kind":"desktop", "launch_path":"C:\\Example.exe", "last_used":null}),
            json!({"bundle_id":null, "kind":null, "launch_path":null, "last_used":null}),
        ] {
            let mut app =
                json!({"pid":7,"name":"Example","running":true,"active":false,"windows":[]});
            app.as_object_mut()
                .unwrap()
                .extend(metadata.as_object().unwrap().clone());
            let output: ListAppsOutput = serde_json::from_value(json!({"apps":[app]})).unwrap();
            assert_eq!(output.apps[0].pid, 7);
        }
    }

    #[test]
    fn native_window_success_shapes_preserve_unknown_pid_and_order() {
        for metadata in [
            json!({"pid":7,"z_index":4,"layer":0,"space_ids":[1],"current_space_id":1,"on_current_space":true}),
            json!({"pid":7,"z_index":0,"layer":0,"minimized":false}),
            json!({"pid":null,"z_index":null,"x":-10,"y":0,"width":100,"height":80}),
        ] {
            let mut window = json!({"window_id":9007199254740993_u64,"app_name":"Example","title":"Document","bounds":{"x":-10,"y":0,"width":100,"height":80},"is_on_screen":true});
            window
                .as_object_mut()
                .unwrap()
                .extend(metadata.as_object().unwrap().clone());
            let output: ListWindowsOutput =
                serde_json::from_value(json!({"windows":[window]})).unwrap();
            assert_eq!(output.windows[0].window_id, 9007199254740993);
        }
        let missing = json!({"windows":[{"window_id":1,"pid":null,"app_name":"Example","title":"Document","bounds":{"x":0,"y":0,"width":100,"height":80},"is_on_screen":true}]});
        assert!(serde_json::from_value::<ListWindowsOutput>(missing).is_err());
    }

    #[test]
    fn window_state_supports_capture_only_and_degraded_native_shapes() {
        let capture: WindowStateOutput = serde_json::from_value(json!({
            "pid":7,"window_id":9,"screenshot_width":100,"screenshot_height":80,
            "screenshot_mime_type":"image/png","screenshot_file_path":"/tmp/example.png",
            "window_bounds":{"x":0,"y":0,"width":100,"height":80},"screenshot_scale":1.0
        }))
        .unwrap();
        assert!(capture.elements.is_none());
        assert!(capture.images.is_empty());
        for value in [
            json!({"pid":7,"window_id":9,"element_count":0,"elements":[],"tree_markdown":"","elements_complete":false,"degraded":true,"degraded_reason":"ax_window_unresolved"}),
            json!({"pid":7,"window_id":9,"element_count":1,"elements":[{"element_index":0,"role":"button","depth":1,"element_token":"s1:0","label":"Apply","frame":{"x":10,"y":20,"w":30,"h":40},"enabled":true}],"elements_complete":false,"screenshot_error":"capture unavailable"}),
            json!({"pid":7,"window_id":9,"elements":[{"element_index":0,"role":"button","depth":1}],"degraded":true,"degraded_reason":"accessibility_window_identity_unproven"}),
        ] {
            let output: WindowStateOutput = serde_json::from_value(value).unwrap();
            assert!(output.elements.is_some());
            output.validate().unwrap();
        }
    }

    #[test]
    fn images_are_envelope_only_and_not_structured_schema_fields() {
        let mut output: WindowStateOutput =
            serde_json::from_value(json!({"pid":7,"window_id":9})).unwrap();
        output.images.push(SnapshotImage {
            mime_type: "image/png".into(),
            data_base64: "aGVsbG8=".into(),
        });
        assert!(serde_json::to_value(&output)
            .unwrap()
            .get("images")
            .is_none());
        assert!(WindowStateOutput::output_schema()["properties"]
            .get("images")
            .is_none());
        assert_eq!(
            GetWindowStateInput::input_schema()["properties"]["max_elements"]["minimum"],
            1
        );
        assert_eq!(
            GetWindowStateInput::input_schema()["properties"]["max_image_dimension"]["minimum"],
            0
        );
        assert_eq!(
            GetWindowStateInput::input_schema()["properties"]["max_dimension"]["minimum"],
            1
        );
        let native_resolution: GetWindowStateInput = serde_json::from_value(json!({
            "pid": 7,
            "window_id": 9,
            "max_image_dimension": 0
        }))
        .unwrap();
        native_resolution.validate().unwrap();
        assert_eq!(ListAppsInput::input_schema()["properties"], json!({}));
    }

    #[test]
    fn invalid_window_observation_metadata_fails_validation() {
        for extra in [
            json!({"screenshot_width":100}),
            json!({"screenshot_width":0,"screenshot_height":80}),
            json!({"screenshot_scale":0}),
            json!({"elements":[],"returned_element_count":1}),
            json!({"total_element_count":1,"returned_element_count":2}),
        ] {
            let mut value = json!({"pid":7,"window_id":9});
            value
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let output: WindowStateOutput = serde_json::from_value(value).unwrap();
            assert!(output.validate().is_err());
        }
    }

    #[test]
    fn action_point_and_hit_are_closed_window_relative_records() {
        let point: ActionPoint = serde_json::from_value(
            json!({"x": 10.5, "y": 20.0, "coordinate_space": "window_points"}),
        )
        .unwrap();
        assert_eq!(point.coordinate_space.as_deref(), Some("window_points"));
        assert_eq!(
            serde_json::to_value(&point).unwrap(),
            json!({"x": 10.5, "y": 20.0, "coordinate_space": "window_points"})
        );
        let bare: ActionPoint = serde_json::from_value(json!({"x": 1, "y": 2})).unwrap();
        assert_eq!(
            serde_json::to_value(&bare).unwrap(),
            json!({"x": 1.0, "y": 2.0})
        );
        assert!(serde_json::from_value::<ActionPoint>(json!({"x": 1})).is_err());
        assert!(
            serde_json::from_value::<ActionPoint>(json!({"x": 1, "y": 2, "unit": "px"})).is_err(),
            "the public point is closed; legacy unit is mapped by the dispatch seam"
        );

        let hit: ActionHit = serde_json::from_value(json!({
            "index": 4, "role": "AXButton", "label": "OK", "identifier": "ok-button"
        }))
        .unwrap();
        assert_eq!(hit.index, Some(4));
        assert_eq!(hit.label.as_deref(), Some("OK"));
        assert_eq!(
            serde_json::to_value(ActionHit::default()).unwrap(),
            json!({})
        );
        assert!(serde_json::from_value::<ActionHit>(json!({"name": "OK"})).is_err());
    }

    #[test]
    fn list_windows_schema_documents_closure_metadata_without_changing_the_record() {
        let schema = crate::advertised_tool_output_schema("list_windows").expect("schema");
        let schema = &schema["anyOf"][0];
        let properties = &schema["properties"];
        assert_eq!(properties["enumeration_complete"]["type"], "boolean");
        assert_eq!(properties["authoritative_window_ids"]["type"], "array");
        assert_eq!(
            properties["authoritative_window_ids"]["items"],
            json!({"type": "integer", "minimum": 1})
        );
        let required = schema["required"].as_array().cloned().unwrap_or_default();
        assert!(!required.contains(&json!("enumeration_complete")));
        assert!(!required.contains(&json!("authoritative_window_ids")));

        // The typed record keeps its layout (UniFFI bindings stay compatible)
        // and accepts the additive keys.
        let payload = json!({
            "windows": [],
            "current_space_id": 3,
            "enumeration_complete": true,
            "authoritative_window_ids": [41, 42]
        });
        assert!(crate::validate_success_output("list_windows", payload.clone()).unwrap());
        let output: ListWindowsOutput = serde_json::from_value(payload).unwrap();
        assert_eq!(output.current_space_id, Some(3));
    }
}
