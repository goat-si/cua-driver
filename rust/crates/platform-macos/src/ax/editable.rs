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

use crate::ax::bindings::{
    copy_bool_attr, copy_children, copy_element_attr, copy_string_attr, is_attribute_settable,
    AXUIElementRef,
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
#[derive(Debug, Clone, Default)]
pub struct ElementFacts {
    pub role: Option<String>,
    pub focused: Option<bool>,
    pub value_settable: Option<bool>,
    pub editable: Option<bool>,
    /// Ancestor roles, nearest parent first.
    pub ancestor_roles: Vec<String>,
    /// A focused AXTextField/AXTextArea exists inside this element.
    pub focused_text_descendant: bool,
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
    /// Role or state could not be read: fail closed.
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
    if !in_table(role, &facts.ancestor_roles) {
        return CellVerdict::NotCell;
    }
    if is_text_role(role) && facts.focused == Some(true) || facts.focused_text_descendant {
        return CellVerdict::Editing;
    }
    CellVerdict::NotEditing
}

/// Read the facts for one element. Ancestor and descendant walks are
/// bounded; every AX read inherits the caller's messaging timeout.
///
/// # Safety
/// The element reference must be valid and live, owned by the caller.
pub unsafe fn facts_for_element(element: AXUIElementRef) -> Option<ElementFacts> {
    if element.is_null() {
        return None;
    }
    let mut facts = ElementFacts {
        role: copy_string_attr(element, "AXRole"),
        focused: copy_bool_attr(element, "AXFocused"),
        value_settable: Some(is_attribute_settable(element, "AXValue")),
        editable: copy_bool_attr(element, "AXEditable"),
        ..Default::default()
    };
    let mut current = element;
    for _ in 0..12 {
        let Some(parent) = copy_element_attr(current, "AXParent") else {
            break;
        };
        if current != element {
            CFRelease(current as CFTypeRef);
        }
        if let Some(role) = copy_string_attr(parent, "AXRole") {
            facts.ancestor_roles.push(role);
        }
        current = parent;
    }
    if current != element {
        CFRelease(current as CFTypeRef);
    }
    facts.focused_text_descendant = has_focused_text_descendant(element, 0);
    Some(facts)
}

/// # Safety
/// The element reference must be valid and live, owned by the caller.
unsafe fn has_focused_text_descendant(element: AXUIElementRef, depth: usize) -> bool {
    if depth >= 3 {
        return false;
    }
    for child in copy_children(element) {
        let found = copy_string_attr(child, "AXRole")
            .as_deref()
            .is_some_and(is_text_role)
            && copy_bool_attr(child, "AXFocused") == Some(true);
        let found = found || has_focused_text_descendant(child, depth + 1);
        CFRelease(child as CFTypeRef);
        if found {
            return true;
        }
    }
    false
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
        // Focused text field with an unsettable value is allowed.
        let mut f = facts("AXTextField");
        f.focused = Some(true);
        f.value_settable = Some(false);
        assert!(!token_refused(&f));
        // Settable value alone is allowed.
        let mut f = facts("AXTextField");
        f.value_settable = Some(true);
        assert!(!token_refused(&f));
        // AXEditable alone is allowed.
        let mut f = facts("AXTextArea");
        f.editable = Some(true);
        assert!(!token_refused(&f));
        // All three negative: refused.
        assert!(token_refused(&facts("AXTextField")));
        // Unreadable attributes fail closed for a text role.
        let mut f = facts("AXTextField");
        f.focused = Some(false);
        assert!(token_refused(&f));
        // Non-text roles are not covered by this guard.
        assert!(!token_refused(&facts("AXButton")));
        assert!(!token_refused(&ElementFacts::default()));
    }

    #[test]
    fn a_focused_or_editable_cell_is_not_an_editor() {
        // Focus alone on a cell is not an editing session.
        assert_eq!(cell_verdict(&cell("AXCell", true)), CellVerdict::NotEditing);
        // AXEditable on the cell is not an editing session either.
        let mut f = cell("AXCell", false);
        f.editable = Some(true);
        assert_eq!(cell_verdict(&f), CellVerdict::NotEditing);
        // Selected row, same.
        assert_eq!(cell_verdict(&cell("AXRow", true)), CellVerdict::NotEditing);
    }

    #[test]
    fn an_inline_editor_in_session_is_editing() {
        // The cell itself is a focused text field.
        assert_eq!(
            cell_verdict(&cell("AXTextField", true)),
            CellVerdict::Editing
        );
        // An unfocused text field in a cell is not in session.
        assert_eq!(
            cell_verdict(&cell("AXTextField", false)),
            CellVerdict::NotEditing
        );
        // A focused text-field descendant of a static-text cell counts.
        let mut f = cell("AXStaticText", false);
        f.focused_text_descendant = true;
        assert_eq!(cell_verdict(&f), CellVerdict::Editing);
    }

    #[test]
    fn non_cells_and_unreadable_states() {
        // An ordinary text field outside a table is not guarded.
        let mut f = facts("AXTextField");
        f.focused = Some(false);
        assert_eq!(cell_verdict(&f), CellVerdict::NotCell);
        // Unreadable role fails closed.
        assert_eq!(
            cell_verdict(&ElementFacts::default()),
            CellVerdict::Unreadable
        );
        // A group inside a table is still guarded.
        let mut f = facts("AXGroup");
        f.ancestor_roles = vec!["AXCell".into(), "AXRow".into(), "AXTable".into()];
        assert_eq!(cell_verdict(&f), CellVerdict::NotEditing);
    }
}
