//! Per-call origin scope for browser page tools.
//!
//! A caller may narrow one page-tool call to the origins it intends to work
//! on with `origin_scope`. Every point that authorizes the live top-level
//! origin then requires that origin to be in the scope as well as in the
//! capability manifest. Attachments to a person's existing browser profile
//! require a scope on every page-tool call; driver-owned and embedded
//! browsers keep working without one.

use std::collections::BTreeSet;

use serde_json::{json, Value};

use super::refusal::{BrowserRefusal, BrowserRefusalCode};
use super::types::EndpointAccessClass;
use crate::protocol::ToolResult;

/// Largest accepted `origin_scope` array.
pub const MAX_ORIGIN_SCOPE: usize = 64;

/// The browser tools that read or act on one page and accept `origin_scope`.
pub const PAGE_TOOLS: [&str; 9] = [
    "get_browser_state",
    "browser_navigate",
    "browser_click",
    "browser_type",
    "browser_pointer",
    "browser_select",
    "browser_dialog",
    "browser_set_input_files",
    "browser_download",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OriginScope {
    origins: BTreeSet<String>,
}

impl OriginScope {
    /// Parse the optional `origin_scope` argument. Entries use the capability
    /// manifest's exact canonical origin form; repeats collapse.
    pub(crate) fn from_args(args: &Value) -> Result<Option<Self>, String> {
        let raw = match args.get("origin_scope") {
            None | Some(Value::Null) => return Ok(None),
            Some(Value::Array(raw)) => raw,
            Some(_) => return Err("origin_scope must be an array of origins".to_owned()),
        };
        if raw.is_empty() || raw.len() > MAX_ORIGIN_SCOPE {
            return Err(format!(
                "origin_scope must list between 1 and {MAX_ORIGIN_SCOPE} origins"
            ));
        }
        let mut origins = BTreeSet::new();
        for entry in raw {
            let entry = entry
                .as_str()
                .ok_or_else(|| "origin_scope entries must be strings".to_owned())?;
            let origin = crate::session_manifest::canonical_origin(entry)
                .map_err(|error| format!("origin_scope: {error}"))?;
            if origin != entry.trim().trim_end_matches('/') {
                return Err(format!(
                    "origin_scope entry '{entry}' must be an exact origin without a path, for example https://app.example"
                ));
            }
            origins.insert(origin);
        }
        Ok(Some(Self { origins }))
    }

    /// Whether the origin of `url` is in this scope.
    pub(crate) fn allows_url(&self, url: &str) -> bool {
        crate::session_manifest::canonical_origin(url)
            .is_ok_and(|origin| self.origins.contains(&origin))
    }
}

/// Parse `origin_scope` for a tool's own invoke, as an argument error.
pub(crate) fn parse(args: &Value) -> Result<Option<OriginScope>, ToolResult> {
    OriginScope::from_args(args).map_err(invalid_argument)
}

fn invalid_argument(detail: String) -> ToolResult {
    ToolResult::error(detail.clone()).with_structured(json!({
        "code": "invalid_arguments",
        "detail": detail,
    }))
}

/// The JSON schema of `origin_scope`.
pub(crate) fn schema() -> Value {
    json!({
        "type": "array",
        "minItems": 1,
        "maxItems": MAX_ORIGIN_SCOPE,
        "items": { "type": "string" },
        "description": "Origins this call may read or act on, for example [\"https://app.example\"]. \
            When present, the tab's live top-level origin (and a navigation's destination) must be \
            in this list as well as in the capability manifest, otherwise the call is refused with \
            browser_origin_outside_scope. Required for a browser attached with an existing \
            profile (browser_origin_scope_required)."
    })
}

/// Checks that need no browser state, run by the dispatcher before the
/// capability manifest's own admission: a malformed `origin_scope`, and a
/// navigation whose destination is outside the scope.
pub fn pre_admission_refusal(tool: &str, args: &Value) -> Option<ToolResult> {
    if !PAGE_TOOLS.contains(&tool) {
        return None;
    }
    let scope = match OriginScope::from_args(args) {
        Ok(scope) => scope?,
        Err(detail) => return Some(invalid_argument(detail)),
    };
    if tool != "browser_navigate" {
        return None;
    }
    let destination = args.get("url").and_then(Value::as_str)?;
    authorize_navigation_destination(Some(&scope), destination)
        .err()
        .map(|refusal| refusal.to_tool_result())
}

/// A navigation destination must be inside the call's scope.
pub(crate) fn authorize_navigation_destination(
    scope: Option<&OriginScope>,
    destination: &str,
) -> Result<(), BrowserRefusal> {
    match scope {
        Some(scope) if !scope.allows_url(destination) => Err(BrowserRefusal::new(
            BrowserRefusalCode::BrowserOriginOutsideScope,
            "the navigation destination is outside this call's origin_scope; nothing was sent",
        )),
        _ => Ok(()),
    }
}

/// An existing-profile attachment is reachable only from a top-level call
/// (never a `run_steps` step or a `run_script` action) and requires a scope
/// on every page-tool call.
pub(crate) fn require_scope(
    access: EndpointAccessClass,
    scope: Option<&OriginScope>,
) -> Result<(), BrowserRefusal> {
    if access == EndpointAccessClass::ExistingProfileApproved && crate::step_pacing::in_batch() {
        return Err(BrowserRefusal::new(
            BrowserRefusalCode::BrowserBatchRefused,
            "pages in the person's own browser profile are reachable only by direct calls, \
             not from run_steps or run_script. Nothing was sent.",
        ));
    }
    if access == EndpointAccessClass::ExistingProfileApproved && scope.is_none() {
        return Err(BrowserRefusal::new(
            BrowserRefusalCode::BrowserOriginScopeRequired,
            "this browser is attached with the person's existing profile; pass origin_scope \
             with the origins this call may read or act on. Nothing was sent.",
        ));
    }
    Ok(())
}

/// The live top-level document must be inside the call's scope.
pub(crate) fn authorize_live_url(
    scope: Option<&OriginScope>,
    live_url: &str,
) -> Result<(), BrowserRefusal> {
    match scope {
        Some(scope) if !scope.allows_url(live_url) => Err(BrowserRefusal::new(
            BrowserRefusalCode::BrowserOriginOutsideScope,
            "the tab's live origin is outside this call's origin_scope; nothing was read or sent",
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_entries_use_the_manifest_origin_form_and_collapse_repeats() {
        let scope = OriginScope::from_args(&json!({
            "origin_scope": ["https://app.example", "https://app.example/", "http://localhost:8080"]
        }))
        .unwrap()
        .unwrap();
        assert_eq!(scope.origins.len(), 2);
        assert!(scope.allows_url("https://app.example/inbox?x=1"));
        assert!(scope.allows_url("http://localhost:8080/"));
        assert!(!scope.allows_url("https://evil.example/"));
        assert!(!scope.allows_url("https://app.example.evil.example/"));
        assert!(OriginScope::from_args(&json!({})).unwrap().is_none());
        // about:blank is an origin like any other, as in the manifest.
        let blank = OriginScope::from_args(&json!({ "origin_scope": ["about:blank"] }))
            .unwrap()
            .unwrap();
        assert!(blank.allows_url("about:blank"));
        assert!(!blank.allows_url("https://app.example/"));
    }

    #[test]
    fn malformed_scopes_are_argument_errors() {
        let too_many: Vec<String> = (0..65).map(|i| format!("https://h{i}.example")).collect();
        for bad in [
            json!([]),
            json!("https://app.example"),
            json!(["https://app.example/path"]),
            json!(["ftp://app.example"]),
            json!([7]),
            json!(too_many),
        ] {
            assert!(
                OriginScope::from_args(&json!({ "origin_scope": bad })).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn pre_admission_refuses_a_destination_outside_the_scope_before_manifest_checks() {
        let refused = pre_admission_refusal(
            "browser_navigate",
            &json!({"url": "https://other.example/", "origin_scope": ["https://app.example"]}),
        )
        .expect("refused");
        assert_eq!(
            refused.structured_content.unwrap()["refusal"]["code"],
            "browser_origin_outside_scope"
        );
        assert!(pre_admission_refusal(
            "browser_navigate",
            &json!({"url": "https://app.example/a", "origin_scope": ["https://app.example"]}),
        )
        .is_none());
        assert!(
            pre_admission_refusal("browser_navigate", &json!({"url": "https://x.example"}))
                .is_none()
        );
        assert!(pre_admission_refusal("click", &json!({"origin_scope": 5})).is_none());
        assert!(pre_admission_refusal("browser_click", &json!({"origin_scope": []})).is_some());
    }
}
