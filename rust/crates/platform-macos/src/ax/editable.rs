//! Editing-state classification for the not_editable pre-dispatch guard
//! (handoff 030 section 3.7). Two separate rules:
//!
//! - TOKEN rule (element_token type_text/set_value): a text-role element is
//!   refused only when it is neither AXFocused nor settable AXValue nor
//!   AXEditable, evaluated after the focus attempt.
//! - CELL rule (untargeted and point-targeted typing): a focused or selected
//!   cell in a row/outline/table is not an editor. Typing is allowed only
//!   with an inline editing session: the cell itself is a focused text
//!   field, or a focused text-field descendant exists.
//!
//! Unreadable or truncated ancestry, and an unfinished descendant scan,
//! fail closed: they never classify as NotCell or Editing.

use crate::ax::bindings::{
    copy_bool_attr, copy_children, copy_element_attr, copy_string_attr, is_attribute_settable,
    AXUIElementRef, AXUIElementSetMessagingTimeout,
};
use core_foundation::base::{CFRelease, CFTypeRef};

/// Whether a role is a text-input role for the token rule.
pub fn is_text_role(role: &str) -> bool {
    matches!(
        role,
        "AXTextField" | "AXTextArea" | "AXTextView" | "AXComboBox" | "AXSearchField"
    )
}

/// Facts read from an element and its ancestry. None means unreadable;
/// every rule fails closed on unreadable state.
#[derive(Debug, Clone)]
pub struct ElementFacts {
    pub role: Option<String>,
    pub focused: Option<bool>,
    pub value_settable: Option<bool>,
    pub editable: Option<bool>,
    /// Ancestor roles, nearest parent first.
    pub ancestor_roles: Vec<String>,
    /// A focused AXTextField/AXTextArea exists inside this element.
    pub focused_text_descendant: bool,
    /// The ancestor walk reached a window/application root with every role
    /// read. False on a failed parent read, an unreadable parent role, or
    /// truncation: the element may hide an unclassified cell ancestor.
    pub ancestry_complete: bool,
    /// The bounded descendant scan finished. False when the work budget ran
    /// out: a focused editor may exist that was not seen.
    pub descendant_scan_complete: bool,
}

impl Default for ElementFacts {
    fn default() -> Self {
        Self {
            role: None,
            focused: None,
            value_settable: None,
            editable: None,
            ancestor_roles: Vec::new(),
            focused_text_descendant: false,
            ancestry_complete: true,
            descendant_scan_complete: true,
        }
    }
}

/// TOKEN rule: refuse only when all three editing indicators are negative.
pub fn token_refused(facts: &ElementFacts) -> bool {
    facts.role.as_deref().is_some_and(is_text_role)
        && facts.focused != Some(true)
        && facts.value_settable != Some(true)
        && facts.editable != Some(true)
}

/// CELL rule verdict for untargeted/point typing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellVerdict {
    /// Not a cell in a row/outline/table: the guard does not apply.
    NotCell,
    /// An inline editing session exists.
    Editing,
    /// A cell with no editing session: typing must be refused.
    NotEditing,
    /// Role, ancestry or state could not be fully read: fail closed.
    Unreadable,
}

fn in_table(role: &str, ancestors: &[String]) -> bool {
    matches!(role, "AXCell" | "AXRow")
        || ancestors
            .iter()
            .any(|r| matches!(r.as_str(), "AXCell" | "AXRow" | "AXOutline" | "AXTable"))
}

pub fn cell_verdict(facts: &ElementFacts) -> CellVerdict {
    let Some(role) = facts.role.as_deref() else {
        return CellVerdict::Unreadable;
    };
    if !facts.ancestry_complete {
        return CellVerdict::Unreadable;
    }
    if !in_table(role, &facts.ancestor_roles) {
        return CellVerdict::NotCell;
    }
    if is_text_role(role) && facts.focused == Some(true) {
        return CellVerdict::Editing;
    }
    if facts.focused_text_descendant {
        return CellVerdict::Editing;
    }
    if !facts.descendant_scan_complete {
        return CellVerdict::Unreadable;
    }
    CellVerdict::NotEditing
}

/// Total nodes the descendant scan may visit before failing closed.
const MAX_SCAN_NODES: usize = 48;
const MAX_SCAN_DEPTH: usize = 3;
const MAX_ANCESTORS: usize = 12;
const AX_TIMEOUT_SECONDS: f32 = 0.5;

/// Read the facts for one element. Walks and the descendant scan are
/// bounded; every AX element read gets a 0.5 s messaging timeout so a busy
/// app cannot stall the guard indefinitely.
///
/// # Safety
/// The element reference must be valid and live, owned by the caller.
pub unsafe fn facts_for_element(element: AXUIElementRef) -> Option<ElementFacts> {
    if element.is_null() {
        return None;
    }
    let _ = AXUIElementSetMessagingTimeout(element, AX_TIMEOUT_SECONDS);
    let mut facts = ElementFacts {
        role: copy_string_attr(element, "AXRole"),
        focused: copy_bool_attr(element, "AXFocused"),
        value_settable: Some(is_attribute_settable(element, "AXValue")),
        editable: copy_bool_attr(element, "AXEditable"),
        ..Default::default()
    };
    let mut current = element;
    let mut complete = false;
    for _ in 0..MAX_ANCESTORS {
        let Some(parent) = copy_element_attr(current, "AXParent") else {
            break;
        };
        if current != element {
            CFRelease(current as CFTypeRef);
        }
        let _ = AXUIElementSetMessagingTimeout(parent, AX_TIMEOUT_SECONDS);
        match copy_string_attr(parent, "AXRole") {
            Some(role) => {
                complete = matches!(role.as_str(), "AXWindow" | "AXApplication");
                facts.ancestor_roles.push(role);
            }
            None => {
                // An unreadable parent role is not the end of the walk: the
                // element may be an unclassified descendant of a cell.
                facts.ancestry_complete = false;
                CFRelease(parent as CFTypeRef);
                return Some(facts);
            }
        }
        current = parent;
        if complete {
            break;
        }
    }
    if current != element {
        CFRelease(current as CFTypeRef);
    }
    facts.ancestry_complete = complete;
    // The descendant scan only runs for table candidates: ordinary targets
    // pay no walk at all.
    if let Some(role) = facts.role.as_deref() {
        if in_table(role, &facts.ancestor_roles)
            && !(is_text_role(role) && facts.focused == Some(true))
        {
            let mut budget = MAX_SCAN_NODES;
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(800);
            let (found, finished) = scan_focused_text_descendant(element, 0, &mut budget, deadline);
            facts.focused_text_descendant = found;
            facts.descendant_scan_complete = finished;
        }
    }
    Some(facts)
}

/// Depth/breadth bounded scan. Returns (found, finished): finished is false
/// when the budget ran out. Every child is released, including the
/// unvisited siblings after a match.
///
/// # Safety
/// The element reference must be valid and live, owned by the caller.
unsafe fn scan_focused_text_descendant(
    element: AXUIElementRef,
    depth: usize,
    budget: &mut usize,
    deadline: std::time::Instant,
) -> (bool, bool) {
    if depth >= MAX_SCAN_DEPTH {
        return (false, true);
    }
    let mut found = false;
    let mut finished = true;
    for child in copy_children(element) {
        if !found {
            if *budget == 0 || std::time::Instant::now() >= deadline {
                finished = false;
            } else {
                *budget -= 1;
                let _ = AXUIElementSetMessagingTimeout(child, AX_TIMEOUT_SECONDS);
                found = copy_string_attr(child, "AXRole")
                    .as_deref()
                    .is_some_and(is_text_role)
                    && copy_bool_attr(child, "AXFocused") == Some(true);
                if !found {
                    let (nested, nested_finished) =
                        scan_focused_text_descendant(child, depth + 1, budget, deadline);
                    found = nested;
                    finished = nested_finished;
                }
            }
        }
        CFRelease(child as CFTypeRef);
    }
    (found, finished)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(role: &str) -> ElementFacts {
        ElementFacts {
            role: Some(role.into()),
            ..Default::default()
        }
    }

    fn cell(role: &str, focused: bool) -> ElementFacts {
        ElementFacts {
            role: Some(role.into()),
            focused: Some(focused),
            ancestor_roles: vec!["AXRow".into(), "AXOutline".into()],
            ..Default::default()
        }
    }

    #[test]
    fn token_rule_follows_the_handoff() {
        let mut f = facts("AXTextField");
        f.focused = Some(true);
        f.value_settable = Some(false);
        assert!(!token_refused(&f));
        let mut f = facts("AXTextField");
        f.value_settable = Some(true);
        assert!(!token_refused(&f));
        let mut f = facts("AXTextArea");
        f.editable = Some(true);
        assert!(!token_refused(&f));
        assert!(token_refused(&facts("AXTextField")));
        let mut f = facts("AXTextField");
        f.focused = Some(false);
        assert!(token_refused(&f));
        assert!(!token_refused(&facts("AXButton")));
        assert!(!token_refused(&ElementFacts::default()));
    }

    #[test]
    fn a_focused_or_editable_cell_is_not_an_editor() {
        assert_eq!(cell_verdict(&cell("AXCell", true)), CellVerdict::NotEditing);
        let mut f = cell("AXCell", false);
        f.editable = Some(true);
        assert_eq!(cell_verdict(&f), CellVerdict::NotEditing);
        assert_eq!(cell_verdict(&cell("AXRow", true)), CellVerdict::NotEditing);
    }

    #[test]
    fn an_inline_editor_in_session_is_editing() {
        assert_eq!(
            cell_verdict(&cell("AXTextField", true)),
            CellVerdict::Editing
        );
        assert_eq!(
            cell_verdict(&cell("AXTextField", false)),
            CellVerdict::NotEditing
        );
        let mut f = cell("AXStaticText", false);
        f.focused_text_descendant = true;
        assert_eq!(cell_verdict(&f), CellVerdict::Editing);
    }

    #[test]
    fn non_cells_and_unreadable_states() {
        let mut f = facts("AXTextField");
        f.focused = Some(false);
        assert_eq!(cell_verdict(&f), CellVerdict::NotCell);
        assert_eq!(
            cell_verdict(&ElementFacts {
                ancestry_complete: false,
                ..Default::default()
            }),
            CellVerdict::Unreadable
        );
        let mut f = facts("AXGroup");
        f.ancestor_roles = vec!["AXCell".into(), "AXRow".into(), "AXTable".into()];
        assert_eq!(cell_verdict(&f), CellVerdict::NotEditing);
    }

    #[test]
    fn incomplete_ancestry_and_scans_fail_closed() {
        // A group whose ancestry was truncated may hide a cell ancestor.
        let mut f = facts("AXGroup");
        f.ancestry_complete = false;
        assert_eq!(cell_verdict(&f), CellVerdict::Unreadable);
        // An unfinished descendant scan cannot prove NotEditing.
        let mut f = cell("AXCell", false);
        f.descendant_scan_complete = false;
        assert_eq!(cell_verdict(&f), CellVerdict::Unreadable);
        // A finished scan with no editor is NotEditing.
        let f = cell("AXCell", false);
        assert_eq!(cell_verdict(&f), CellVerdict::NotEditing);
    }
}
