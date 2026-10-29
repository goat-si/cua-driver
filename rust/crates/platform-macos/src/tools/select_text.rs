use crate::goat::action_latency as latency;
use async_trait::async_trait;
use core_foundation::{
    base::{CFGetTypeID, CFRelease, CFTypeRef, TCFType},
    string::CFString,
};
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::Value;
use std::sync::Arc;

use crate::ax::{
    bindings::*,
    tree::{read_selected_text_range, SelectedTextRange},
};

use super::ToolState;

pub struct SelectTextTool {
    state: Arc<ToolState>,
}

impl SelectTextTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "select_text".into(),
        description: "Select a substring in an editable element without changing its AXValue. Requires the exact pid, window_id and current element_token. The needle must be nonempty and unique unless zero-based occurrence is supplied. Offsets and readback use UTF-16 code units; secure fields and ambiguous or unverified selections are refused.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["pid", "window_id", "element_token", "text"],
            "properties": {
                "session": { "type": "string", "description": "Multi-call session label." },
                "pid": { "type": "integer", "description": "Target process ID. Required for exact-window authorization." },
                "window_id": { "type": "integer", "description": "Target CGWindowID. Required for exact-window authorization." },
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "text": { "type": "string", "minLength": 1, "description": "Nonempty substring to select." },
                "occurrence": { "type": "integer", "minimum": 0, "description": "Zero-based occurrence, including overlapping matches. Omit to require exactly one match." }
                ,"prefix": { "type": "string", "description": "Optional exact text immediately before the match." }
                ,"suffix": { "type": "string", "description": "Optional exact text immediately after the match." }
                ,"selection": { "type": "string", "enum": ["text", "cursor_before", "cursor_after"], "description": "Select text (default) or collapse the caret before/after the match." }
            },
            "additionalProperties": false
        }),
        read_only: false,
        destructive: false,
        idempotent: true,
        open_world: true,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum OptionalStringAttr {
    Value(Option<String>),
    Unreadable,
}

unsafe fn optional_string_attr(element: AXUIElementRef, name: &str) -> OptionalStringAttr {
    let attr = CFString::new(name);
    let mut value: CFTypeRef = std::ptr::null();
    let error = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if error == kAXErrorNoValue || error == kAXErrorAttributeUnsupported {
        if !value.is_null() {
            CFRelease(value);
        }
        return OptionalStringAttr::Value(None);
    }
    if error != kAXErrorSuccess || value.is_null() {
        if !value.is_null() {
            CFRelease(value);
        }
        return OptionalStringAttr::Unreadable;
    }
    if CFGetTypeID(value) != CFString::type_id() {
        CFRelease(value);
        return OptionalStringAttr::Unreadable;
    }
    OptionalStringAttr::Value(Some(
        CFString::wrap_under_create_rule(value as _).to_string(),
    ))
}

fn selection_exposure_allowed(attributes: &[OptionalStringAttr; 7]) -> bool {
    if attributes
        .iter()
        .any(|attr| matches!(attr, OptionalStringAttr::Unreadable))
    {
        return false;
    }
    let value = |index: usize| match &attributes[index] {
        OptionalStringAttr::Value(value) => value.as_deref(),
        OptionalStringAttr::Unreadable => None,
    };
    let Some(role) = value(0).filter(|role| !role.trim().is_empty()) else {
        return false;
    };
    !cua_driver_core::snapshot_compaction::is_secure_text_field(
        &cua_driver_core::snapshot_compaction::FieldSecurityMeta {
            role,
            subrole: value(1),
            title: value(2),
            description: value(3),
            placeholder: value(4),
            identifier: value(5),
            help: value(6),
            is_password_flag: false,
        },
    )
}

fn find_occurrences(value: &str, needle: &str) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    let mut matches = Vec::new();
    let mut offset = 0;
    while offset + needle.len() <= value.len() {
        let Some(relative) = value[offset..].find(needle) else {
            break;
        };
        let start = offset + relative;
        matches.push((start, start + needle.len()));
        let Some(step) = value[start..].chars().next().map(char::len_utf8) else {
            break;
        };
        offset = start + step;
    }
    matches
}

fn utf16_range(value: &str, needle: &str, start: usize) -> Option<SelectedTextRange> {
    let prefix = value.get(..start)?;
    let location = isize::try_from(prefix.encode_utf16().count()).ok()?;
    let length = isize::try_from(needle.encode_utf16().count()).ok()?;
    Some(SelectedTextRange { location, length })
}

#[repr(C)]
struct CfRange {
    location: isize,
    length: isize,
}

#[derive(Debug)]
struct SelectError {
    message: String,
    post_dispatch: bool,
}

impl SelectError {
    fn pre(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            post_dispatch: false,
        }
    }

    fn post(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            post_dispatch: true,
        }
    }
}

impl From<String> for SelectError {
    fn from(message: String) -> Self {
        Self::pre(message)
    }
}

impl From<&str> for SelectError {
    fn from(message: &str) -> Self {
        Self::pre(message)
    }
}

unsafe fn write_selected_text_range(
    element: AXUIElementRef,
    range: SelectedTextRange,
) -> Result<(), SelectError> {
    let range = CfRange {
        location: range.location,
        length: range.length,
    };
    let value = AXValueCreate(kAXValueCFRangeType, &range as *const CfRange as *const _);
    if value.is_null() {
        return Err("could not allocate the selected-text CFRange".into());
    }
    let attr = CFString::new("AXSelectedTextRange");
    let error =
        AXUIElementSetAttributeValue(element, attr.as_concrete_TypeRef(), value as CFTypeRef);
    CFRelease(value as CFTypeRef);
    (error == kAXErrorSuccess).then_some(()).ok_or_else(|| {
        SelectError::post(format!(
            "AXSelectedTextRange write failed with AXError {error}"
        ))
    })
}

unsafe fn select_text_blocking(
    element: AXUIElementRef,
    needle: &str,
    occurrence: Option<usize>,
    prefix: Option<&str>,
    suffix: Option<&str>,
    selection: &str,
) -> Result<(String, SelectedTextRange, usize, usize), SelectError> {
    let attributes = [
        optional_string_attr(element, "AXRole"),
        optional_string_attr(element, "AXSubrole"),
        optional_string_attr(element, "AXTitle"),
        optional_string_attr(element, "AXDescription"),
        optional_string_attr(element, "AXPlaceholderValue"),
        optional_string_attr(element, "AXIdentifier"),
        optional_string_attr(element, "AXHelp"),
    ];
    if !selection_exposure_allowed(&attributes) {
        return Err("select_text refuses secure fields or unreadable security metadata".into());
    }
    if !is_attribute_settable(element, "AXSelectedTextRange") {
        return Err("AXSelectedTextRange is not settable on this element".into());
    }
    let before = copy_string_attr(element, "AXValue")
        .ok_or_else(|| "AXValue is unreadable; selection was not attempted".to_owned())?;
    let matches: Vec<(usize, usize)> = find_occurrences(&before, needle)
        .into_iter()
        .filter(|(start, end)| {
            prefix.is_none_or(|prefix| before[..*start].ends_with(prefix))
                && suffix.is_none_or(|suffix| before[*end..].starts_with(suffix))
        })
        .collect();
    let match_count = matches.len();
    let selected = match occurrence {
        Some(index) => matches
            .get(index)
            .copied()
            .map(|range| (index, range))
            .ok_or_else(|| {
                format!("occurrence {index} is out of range; found {match_count} matches")
            })?,
        None if match_count == 1 => (0, matches[0]),
        None => {
            return Err(format!(
                "needle must match exactly once unless occurrence is supplied; found {match_count} matches"
            ).into())
        }
    };
    let (occurrence, (start, _)) = selected;
    let mut range = utf16_range(&before, needle, start)
        .ok_or_else(|| "selected range exceeds the native CFRange representation".to_owned())?;
    match selection {
        "text" => {}
        "cursor_before" => range.length = 0,
        "cursor_after" => {
            range.location = range
                .location
                .checked_add(range.length)
                .ok_or_else(|| "caret offset exceeds CFRange".to_owned())?;
            range.length = 0;
        }
        _ => return Err("selection must be text, cursor_before or cursor_after".into()),
    }
    write_selected_text_range(element, range)?;
    let after = copy_string_attr(element, "AXValue").ok_or_else(|| {
        SelectError::post("AXValue is unreadable after selection; outcome is unverifiable")
    })?;
    if after != before {
        return Err(SelectError::post(
            "AXValue changed during selection; refusing to report success",
        ));
    }
    let actual_range = read_selected_text_range(element)
        .ok_or_else(|| SelectError::post("AXSelectedTextRange readback is unavailable"))?;
    if actual_range != range {
        return Err(SelectError::post(format!(
            "selection range readback mismatch: expected {range:?}, got {actual_range:?}"
        )));
    }
    let selected_text = copy_string_attr(element, "AXSelectedText")
        .ok_or_else(|| SelectError::post("AXSelectedText readback is unavailable"))?;
    let expected_text = if selection == "text" { needle } else { "" };
    if selected_text != expected_text {
        return Err(SelectError::post(
            "selected-text readback differs from the requested needle",
        ));
    }
    Ok((selected_text, range, occurrence, match_count))
}

#[async_trait]
impl Tool for SelectTextTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let pid = match args.require_i32("pid") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let window_id = match args.require_u32("window_id") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let needle = match args.require_str("text") {
            Ok(value) if !value.is_empty() => value,
            Ok(_) => return super::invalid_arguments("select_text text must be nonempty"),
            Err(error) => return error,
        };
        if args.get("element_token").is_none() {
            return super::invalid_arguments("select_text requires element_token");
        }
        let occurrence = args.opt_u64("occurrence").map(|value| value as usize);
        let prefix = args.opt_str("prefix");
        let suffix = args.opt_str("suffix");
        let selection = args.opt_str("selection").unwrap_or_else(|| "text".into());
        if !matches!(
            selection.as_str(),
            "text" | "cursor_before" | "cursor_after"
        ) {
            return super::invalid_arguments(
                "selection must be text, cursor_before or cursor_after",
            );
        }
        let resolved =
            match latency::resolve(|| self.state.snapshots.resolve(pid, &args)) {
                Ok(cua_driver_core::element_token::ResolvedElement::Element {
                    window_id: resolved_window,
                    element,
                    ..
                }) if resolved_window == u64::from(window_id) => element,
                Ok(_) => return super::invalid_arguments(
                    "select_text pid/window_id and element_token must identify the same element",
                ),
                Err(error) => return error,
            };
        let _mutation_lease = match super::gate_background_window_action(
            pid,
            window_id,
            Some(resolved.as_ptr()),
            cua_driver_core::background_input::BackgroundAction::AxSemantic,
        )
        .await
        {
            Ok(lease) => lease,
            Err(error) => return error,
        };
        let element = resolved.clone();
        let result = latency::spawn_action_blocking(move || unsafe {
            select_text_blocking(
                element.as_ptr() as AXUIElementRef,
                &needle,
                occurrence,
                prefix.as_deref(),
                suffix.as_deref(),
                &selection,
            )
        })
        .await;
        match result {
            Ok(Ok((selected_text, range, occurrence, match_count))) => ToolResult::text(format!(
                "Selected occurrence {occurrence} in UTF-16 range {}+{} without changing AXValue.",
                range.location, range.length
            ))
            .with_structured(serde_json::json!({
                "effect": "confirmed",
                "verified": true,
                "selected_text": selected_text,
                "selected_text_range": {
                    "location": range.location,
                    "length": range.length,
                    "units": "utf16"
                },
                "occurrence": occurrence,
                "match_count": match_count,
                "value_changed": false
            })),
            Ok(Err(error)) => {
                let code =
                    if error.message.contains("secure") || error.message.contains("not settable") {
                        "cannot_select_text_for_element"
                    } else if error.message.contains("occurrence") {
                        "text_occurrence_out_of_range"
                    } else if error.message.contains("matches") {
                        "text_to_select_ambiguous"
                    } else if error.message.contains("readback")
                        || error.message.contains("AXValue changed")
                    {
                        "selection_readback_mismatch"
                    } else {
                        "text_to_select_not_found"
                    };
                ToolResult::error(format!("select_text failed: {}", error.message)).with_structured(
                    serde_json::json!({
                        "code": code,
                        "effect": if error.post_dispatch { "unverifiable" } else { "refused" }
                    }),
                )
            }
            Err(error) => ToolResult::error(format!("select_text task failed: {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occurrences_overlap_on_scalar_boundaries() {
        assert_eq!(find_occurrences("aaa", "aa"), vec![(0, 2), (1, 3)]);
        assert_eq!(find_occurrences("a😀a😀", "😀"), vec![(1, 5), (6, 10)]);
        assert!(find_occurrences("abc", "").is_empty());
    }

    #[test]
    fn utf16_offsets_include_surrogates_and_combining_scalars() {
        let value = "가😀e\u{301}가";
        let start = value.find("e\u{301}").unwrap();
        assert_eq!(
            utf16_range(value, "e\u{301}", start),
            Some(SelectedTextRange {
                location: 3,
                length: 2
            })
        );
        let start = value.rfind('가').unwrap();
        assert_eq!(
            utf16_range(value, "가", start),
            Some(SelectedTextRange {
                location: 5,
                length: 1
            })
        );
    }

    #[test]
    fn security_metadata_fails_closed() {
        let ordinary: [OptionalStringAttr; 7] = std::array::from_fn(|index| {
            OptionalStringAttr::Value((index == 0).then(|| "AXTextField".into()))
        });
        assert!(selection_exposure_allowed(&ordinary));
        let mut secure = ordinary.clone();
        secure[0] = OptionalStringAttr::Value(Some("AXSecureTextField".into()));
        assert!(!selection_exposure_allowed(&secure));
        let mut unreadable = ordinary;
        unreadable[6] = OptionalStringAttr::Unreadable;
        assert!(!selection_exposure_allowed(&unreadable));
    }
}
