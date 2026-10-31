use cua_driver_core::snapshot_compaction::{
    is_secure_text_field, redact_if_secure, redact_mirrored_secure_name, FieldSecurityMeta,
};

pub(crate) struct RawNodeText<'a> {
    pub control_type: &'a str,
    pub name: Option<String>,
    pub value: Option<String>,
    pub help: Option<String>,
    pub description: Option<String>,
    pub automation_id: Option<String>,
    pub is_password: Option<bool>,
    pub has_value_pattern: bool,
}

#[derive(Debug)]
pub(crate) struct ProjectedNodeText {
    pub name: Option<String>,
    pub value: Option<String>,
    pub help: Option<String>,
    pub secure: bool,
}

pub(crate) fn treat_as_secure_flag(
    is_password: Option<bool>,
    control_type: &str,
    has_value_pattern: bool,
    has_value_text: bool,
) -> bool {
    is_password.unwrap_or(
        matches!(control_type, "Edit" | "Document") || has_value_pattern || has_value_text,
    )
}

pub(crate) fn withhold_secure_help(secure: bool, help: Option<String>) -> Option<String> {
    if secure {
        None
    } else {
        help
    }
}

pub(crate) fn msaa_name_withheld(role: i32, state: Option<u32>) -> bool {
    match state {
        Some(state) => state & 0x2000_0000 != 0,
        None => role == 0x2a,
    }
}

pub(crate) fn project_node_text(raw: RawNodeText<'_>) -> ProjectedNodeText {
    let meta = FieldSecurityMeta {
        role: raw.control_type,
        subrole: None,
        title: raw.name.as_deref(),
        description: raw.description.as_deref(),
        placeholder: None,
        identifier: raw.automation_id.as_deref(),
        help: raw.help.as_deref(),
        is_password_flag: treat_as_secure_flag(
            raw.is_password,
            raw.control_type,
            raw.has_value_pattern,
            raw.value.is_some(),
        ),
    };
    let secure = is_secure_text_field(&meta);
    let (value, _) = redact_if_secure(&meta, raw.value.clone(), None);
    let name = redact_mirrored_secure_name(secure, raw.name, raw.value.as_deref());
    ProjectedNodeText {
        name,
        value,
        help: withhold_secure_help(secure, raw.help),
        secure,
    }
}

#[cfg(test)]
#[path = "secure_field_tests.rs"]
mod tests;
