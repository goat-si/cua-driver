//! What one AT-SPI node shows of its text: its name and its value, and
//! whether it is a password field whose typed text must never be shown.
//! Pure, so the decisions are tested on every host; the native walk that
//! reads these inputs over D-Bus is Linux only.

/// AT-SPI `ATSPI_ROLE_PASSWORD_TEXT` (atspi-constants.h; `atspi::Role::PasswordText`).
pub(crate) const ROLE_PASSWORD_TEXT: u32 = 40;

/// Whether a node is a password field. GTK4 names role 40 (`PASSWORD_TEXT`)
/// "text box", so the role name alone misses it.
/// The numeric role decides first; the role name (translated, toolkit-chosen)
/// still counts when the number is unavailable or says otherwise.
pub(crate) fn is_password_field(role: Option<u32>, role_lower: &str) -> bool {
    role == Some(ROLE_PASSWORD_TEXT) || role_lower.contains("password")
}

/// The `value` of one walked node: its Value-interface value, else, for an
/// editable text widget without a Value interface, its non-empty Text
/// content (the typed string). A SpinButton implements Text and Value, so it
/// keeps its numeric value. Callers pass `has_editable: false` for password
/// fields.
pub(crate) fn editable_text_value(
    has_editable: bool,
    has_value: bool,
    value: Option<String>,
    text_content: &str,
) -> Option<String> {
    if value.is_some() || has_value || !has_editable || text_content.is_empty() {
        return value;
    }
    Some(text_content.to_owned())
}

/// The display name of a widget whose accessible name is empty: its Text
/// content, for text widgets. Runs after the `LABELLED_BY` lookup so a
/// labelled entry is named by its label and its typed text stays in `value`.
/// A value control (GtkSpinButton implements AtkText with its formatted
/// number) would otherwise be named after its own value, and a password
/// field's content is never surfaced, not even when the toolkit itself set
/// the accessible name to it.
pub(crate) fn text_content_name(
    name: String,
    is_password: bool,
    has_value: bool,
    text_content: &str,
) -> String {
    if is_password {
        let typed = text_content.trim();
        return if !typed.is_empty() && name.trim() == typed {
            String::new()
        } else {
            name
        };
    }
    if name.trim().is_empty() && !text_content.trim().is_empty() && !has_value {
        return text_content.to_owned();
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editable_text_reports_its_content_as_value() {
        assert_eq!(
            editable_text_value(true, false, None, "jev-use native note").as_deref(),
            Some("jev-use native note")
        );
    }

    #[test]
    fn empty_or_non_editable_text_has_no_value() {
        assert_eq!(editable_text_value(true, false, None, ""), None);
        assert_eq!(
            editable_text_value(false, false, None, "static label"),
            None
        );
    }

    #[test]
    fn value_interface_wins() {
        assert_eq!(
            editable_text_value(true, true, Some("3".into()), "3.0").as_deref(),
            Some("3")
        );
        assert_eq!(editable_text_value(true, true, None, "3.0"), None);
    }

    #[test]
    fn a_labelled_entry_keeps_its_label_and_a_password_never_shows_its_text() {
        assert_eq!(
            text_content_name("Verification code".into(), false, false, "482913"),
            "Verification code"
        );
        assert_eq!(text_content_name(String::new(), true, false, "hunter2"), "");
        assert_eq!(
            text_content_name(String::new(), false, false, "draft"),
            "draft"
        );
        assert_eq!(text_content_name(String::new(), false, true, "0.0"), "");
    }

    /// What `get_window_state` derives for one walked entry: (name, value).
    fn shown(role: Option<u32>, role_name: &str, text_content: &str) -> (String, Option<String>) {
        let is_password = is_password_field(role, &role_name.to_ascii_lowercase());
        let value = editable_text_value(!is_password, false, None, text_content);
        (
            text_content_name(String::new(), is_password, false, text_content),
            value,
        )
    }

    #[test]
    fn a_gtk4_password_entry_named_text_box_shows_neither_name_nor_value() {
        // GTK 4.14 reports role 40 (PASSWORD_TEXT) with the role name "text box".
        assert_eq!(
            shown(Some(ROLE_PASSWORD_TEXT), "text box", "pw-entry-secret"),
            (String::new(), None)
        );
        // The role name alone, when the number cannot be read.
        assert_eq!(
            shown(None, "password text", "hunter2"),
            (String::new(), None)
        );
        // An ordinary GTK4 text box still shows its text.
        assert_eq!(
            shown(Some(61), "text box", "draft"),
            ("draft".into(), Some("draft".into()))
        );
        // A toolkit that names the field after its content.
        assert_eq!(
            text_content_name("pw-entry-secret".into(), true, false, "pw-entry-secret"),
            ""
        );
        assert_eq!(
            text_content_name("Password".into(), true, false, "hunter2"),
            "Password"
        );
    }
}
