use super::*;

const MARKER: &str = "hunter2";

fn raw(control_type: &str) -> RawNodeText<'_> {
    RawNodeText {
        control_type,
        name: Some(MARKER.into()),
        value: Some(MARKER.into()),
        help: Some(MARKER.into()),
        description: None,
        automation_id: None,
        is_password: Some(false),
        has_value_pattern: false,
    }
}

fn assert_contained(raw: RawNodeText<'_>) {
    let result = project_node_text(raw);
    assert!(result.secure, "expected secure projection: {result:?}");
    for field in [&result.name, &result.value, &result.help] {
        assert!(
            !field.as_deref().unwrap_or_default().contains(MARKER),
            "leaked secret: {result:?}"
        );
    }
}

#[test]
fn secure_field_unknown_edit_contains_secret() {
    let mut input = raw("Edit");
    input.is_password = None;
    assert_contained(input);
}

#[test]
fn secure_field_projection_contains_secrets() {
    let mut input = raw("Edit");
    input.is_password = Some(true);
    assert_contained(input);
    let mut input = raw("Custom");
    input.is_password = None;
    assert_contained(input);
    let mut input = raw("Edit");
    input.automation_id = Some("PasswordBox".into());
    assert_contained(input);
    for value in [None, Some("  ".into())] {
        let mut input = raw("Edit");
        input.is_password = Some(true);
        input.value = value;
        assert_contained(input);
    }
}

#[test]
fn secure_field_flag_table() {
    for control in ["Edit", "Document", "Button"] {
        for pattern in [false, true] {
            for value in [false, true] {
                assert!(treat_as_secure_flag(Some(true), control, pattern, value));
                assert!(!treat_as_secure_flag(Some(false), control, pattern, value));
                assert_eq!(
                    treat_as_secure_flag(None, control, pattern, value),
                    control != "Button" || pattern || value
                );
            }
        }
    }
}

#[test]
fn secure_field_non_secure_preserves_help_and_name() {
    let mut input = raw("Button");
    input.value = None;
    let projected = project_node_text(input);
    assert!(!projected.secure);
    assert_eq!(projected.name.as_deref(), Some(MARKER));
    assert_eq!(projected.help.as_deref(), Some(MARKER));
    let mut input = raw("Button");
    input.name = None;
    input.value = None;
    assert_eq!(project_node_text(input).help.as_deref(), Some(MARKER));
    assert_eq!(withhold_secure_help(true, Some(MARKER.into())), None);
    assert_eq!(
        withhold_secure_help(false, Some(MARKER.into())).as_deref(),
        Some(MARKER)
    );
}

#[test]
fn secure_field_msaa_protected_and_unknown_state() {
    for role in [0x2a, 0x2b] {
        assert!(msaa_name_withheld(role, Some(0x2000_0000)));
        assert!(!msaa_name_withheld(role, Some(0)));
        assert_eq!(msaa_name_withheld(role, None), role == 0x2a);
    }
}
