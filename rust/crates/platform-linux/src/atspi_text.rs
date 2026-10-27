//! What one AT-SPI node shows of its text: its name and its value, and
//! whether it is a password field whose typed text must never be shown.
//! Pure, so the decisions are tested on every host; the native walk that
//! reads these inputs over D-Bus is Linux only.

/// AT-SPI `ATSPI_ROLE_PASSWORD_TEXT` (atspi-constants.h; `atspi::Role::PasswordText`).
pub(crate) const ROLE_PASSWORD_TEXT: u32 = 40;

/// What a write result shows in place of a value it must not repeat.
pub(crate) const REDACTED: &str = "[REDACTED]";

/// The value a write result may repeat back: the requested value, except for
/// a password field, whose value would otherwise land in the reply and the
/// transcript.
pub(crate) fn echoed_value(value: &str, is_password: bool) -> &str {
    if is_password {
        REDACTED
    } else {
        value
    }
}

/// A text widget's numeric AT-SPI role, as far as it could be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RoleRead {
    Known(u32),
    /// Asked and not answered (error or timeout).
    Failed,
    /// Not asked: the node has no Text or EditableText interface.
    NotRead,
}

/// Whether a node is a password field. GTK4 names role 40 (`PASSWORD_TEXT`)
/// "text box", so the role name alone misses it. The numeric role decides
/// first; the role name (translated, toolkit-chosen) still counts. A text
/// widget whose role could not be read is treated as one: its text stays
/// hidden rather than risk showing a password.
pub(crate) fn is_password_field(role: RoleRead, role_lower: &str) -> bool {
    match role {
        RoleRead::Known(number) => number == ROLE_PASSWORD_TEXT || role_lower.contains("password"),
        RoleRead::Failed => true,
        RoleRead::NotRead => role_lower.contains("password"),
    }
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
/// number) would otherwise be named after its own value.
///
/// A password field's content is never surfaced, and its own accessible name
/// may mirror that content. The name is kept only when it came from a
/// `LABELLED_BY` label (`labelled`), or when the whole content was read
/// (`text_complete`) and the name shares nothing with it.
pub(crate) fn text_content_name(
    name: String,
    is_password: bool,
    labelled: bool,
    text_complete: bool,
    has_value: bool,
    text_content: &str,
) -> String {
    if is_password {
        let label = name.trim();
        let typed = text_content.trim();
        let distinct = typed.is_empty() || (!label.contains(typed) && !typed.contains(label));
        return if labelled || (text_complete && distinct) {
            name
        } else {
            String::new()
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
            text_content_name(
                "Verification code".into(),
                false,
                true,
                true,
                false,
                "482913"
            ),
            "Verification code"
        );
        assert_eq!(
            text_content_name(String::new(), true, false, true, false, "hunter2"),
            ""
        );
        assert_eq!(
            text_content_name(String::new(), false, false, true, false, "draft"),
            "draft"
        );
        assert_eq!(
            text_content_name(String::new(), false, false, true, true, "0.0"),
            ""
        );
    }

    /// What `get_window_state` derives for one walked, unlabelled entry whose
    /// Text was read in full: (name, value).
    fn shown(role: RoleRead, role_name: &str, text_content: &str) -> (String, Option<String>) {
        let is_password = is_password_field(role, &role_name.to_ascii_lowercase());
        let value = editable_text_value(!is_password, false, None, text_content);
        let name = text_content_name(String::new(), is_password, false, true, false, text_content);
        (name, value)
    }

    #[test]
    fn a_gtk4_password_entry_named_text_box_shows_neither_name_nor_value() {
        // GTK 4.14 reports role 40 (PASSWORD_TEXT) with the role name "text box".
        assert_eq!(
            shown(
                RoleRead::Known(ROLE_PASSWORD_TEXT),
                "text box",
                "pw-entry-secret"
            ),
            (String::new(), None)
        );
        // The role name alone, when the node has no text interface to ask.
        assert_eq!(
            shown(RoleRead::NotRead, "password text", "hunter2"),
            (String::new(), None)
        );
        // An ordinary GTK4 text box still shows its text.
        assert_eq!(
            shown(RoleRead::Known(61), "text box", "draft"),
            ("draft".into(), Some("draft".into()))
        );
    }

    #[test]
    fn a_text_widget_whose_role_cannot_be_read_hides_its_text() {
        assert_eq!(
            shown(RoleRead::Failed, "text box", "pw-entry-secret"),
            (String::new(), None)
        );
    }

    #[test]
    fn a_password_field_keeps_only_a_name_it_can_prove_is_not_its_content() {
        let name = |name: &str, labelled, complete, typed| {
            text_content_name(name.into(), true, labelled, complete, false, typed)
        };
        // A toolkit that names the field after its content.
        assert_eq!(name("pw-entry-secret", false, true, "pw-entry-secret"), "");
        // Text unread, failed, or cut off at the read cap: the name may be it.
        assert_eq!(name("pw-entry-secret", false, false, ""), "");
        assert_eq!(
            name("pw-entry-secret-and-more", false, false, "pw-entry-secret"),
            ""
        );
        // A distinct name over fully read content, or a LABELLED_BY label.
        assert_eq!(name("Password", false, true, "hunter2"), "Password");
        assert_eq!(name("Password", false, true, ""), "Password");
        assert_eq!(name("Password", true, false, ""), "Password");
    }

    #[test]
    fn a_write_result_never_repeats_a_password() {
        assert_eq!(echoed_value("set-secret-2", true), REDACTED);
        assert_eq!(echoed_value("draft", false), "draft");
    }
}
