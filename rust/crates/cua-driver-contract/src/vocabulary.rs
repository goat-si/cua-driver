// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! Closed cross-platform accessibility vocabulary for the wire contract.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

macro_rules! roles {
    ($($variant:ident),+ $(,)?) => {
        /// A platform-neutral role. Unknown means a successful read with unrecognized evidence.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, uniffi::Enum)]
        #[serde(rename_all = "snake_case")]
        pub enum Role { $($variant),+ }

        impl Role {
            /// Parse a canonical role token, ignoring case and surrounding whitespace.
            /// Both the snake_case wire spelling and its unseparated form are accepted.
            pub fn from_token(value: &str) -> Self {
                let normalized = value.trim().to_ascii_lowercase();
                if let Ok(role) = serde_json::from_value::<Self>(
                    serde_json::Value::String(normalized.clone()),
                ) {
                    return role;
                }
                if !normalized.contains('_') {
                    $(if normalized.eq_ignore_ascii_case(stringify!($variant)) {
                        return Self::$variant;
                    })+
                }
                Self::Unknown
            }
        }
    };
}

roles!(
    Alert,
    AlertDialog,
    Application,
    Article,
    Banner,
    Browser,
    Button,
    Cell,
    Checkbox,
    ColorWell,
    Column,
    ComboBox,
    Complementary,
    ContentInfo,
    DateField,
    Definition,
    Dialog,
    Disclosure,
    DockItem,
    Document,
    Drawer,
    Form,
    Grid,
    Group,
    Handle,
    Heading,
    HelpTag,
    Image,
    Incrementor,
    LayoutItem,
    LevelIndicator,
    Link,
    List,
    ListBox,
    Log,
    Main,
    Marquee,
    Matte,
    Math,
    Menu,
    MenuButton,
    MenuItem,
    Navigation,
    Note,
    Option,
    Outline,
    Page,
    Paragraph,
    Popover,
    ProgressBar,
    RadioButton,
    RadioGroup,
    Region,
    RelevanceIndicator,
    Row,
    Ruler,
    RulerMarker,
    ScrollArea,
    ScrollBar,
    Search,
    Separator,
    Sheet,
    Slider,
    Splitter,
    StaticText,
    Status,
    Switch,
    Tab,
    TabList,
    TabPanel,
    Table,
    TextField,
    Term,
    TimeField,
    Timer,
    Toolbar,
    Tooltip,
    TreeItem,
    WebArea,
    Window,
    Unknown,
);

/// An asserted accessibility state; absence of a token is not a negative assertion.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, uniffi::Enum,
)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Focused,
    Disabled,
    Secure,
    Expanded,
    Checked,
    Selected,
    Hidden,
    Busy,
    Modal,
    Required,
    Indeterminate,
    Pressed,
    Readonly,
    Offscreen,
    Invalid,
    Multiselectable,
    Haspopup,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_tokens_normalize_and_unknown_is_explicit() {
        for (input, expected) in [
            (" BUTTON ", Role::Button),
            ("TextField", Role::TextField),
            (" text_field ", Role::TextField),
            ("alert_dialog", Role::AlertDialog),
            ("WebArea", Role::WebArea),
            ("unknown", Role::Unknown),
            ("not a role", Role::Unknown),
            ("_button", Role::Unknown),
            ("text__field", Role::Unknown),
            ("", Role::Unknown),
        ] {
            assert_eq!(Role::from_token(input), expected, "{input:?}");
        }
    }

    #[test]
    fn state_vocabulary_roundtrips() {
        let tokens = [
            "focused",
            "disabled",
            "secure",
            "expanded",
            "checked",
            "selected",
            "hidden",
            "busy",
            "modal",
            "required",
            "indeterminate",
            "pressed",
            "readonly",
            "offscreen",
            "invalid",
            "multiselectable",
            "haspopup",
        ];
        for token in tokens {
            let state: State = serde_json::from_value(serde_json::json!(token)).unwrap();
            assert_eq!(serde_json::to_value(state).unwrap(), token);
        }
        assert!(serde_json::from_value::<State>(serde_json::json!("unrecognized")).is_err());
    }
}
