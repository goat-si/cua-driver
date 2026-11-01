use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent<'a> {
    Click,
    DoubleClick,
    TripleClick,
    RightClick,
    Hover,
    Drag,
    SetValue,
    TypeText,
    Clear,
    Select,
    Toggle,
    Check,
    Uncheck,
    Expand,
    Collapse,
    Scroll,
    ScrollTo,
    Focus,
    PressKey,
    KeyDown,
    KeyUp,
    Open,
    SelectText,
    PerformAction { name: &'a str },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Requirements {
    pub visible: bool,
    pub stable: bool,
    pub enabled: bool,
    pub editable: bool,
    pub receives_events: bool,
}

impl Intent<'_> {
    pub const fn requirements(self) -> Requirements {
        use Intent::*;
        let (visible, stable, enabled, editable, receives_events) = match self {
            Click | DoubleClick | TripleClick | RightClick => (true, true, true, false, true),
            Hover | Drag => (true, true, false, false, true),
            SetValue | TypeText | Clear => (true, false, true, true, false),
            Select | Open | SelectText | PerformAction { .. } => (true, false, true, false, false),
            Toggle | Check | Uncheck | Expand | Collapse | Scroll => {
                (true, true, true, false, false)
            }
            ScrollTo => (false, true, false, false, false),
            Focus | PressKey | KeyDown | KeyUp => (false, false, false, false, false),
        };
        Requirements {
            visible,
            stable,
            enabled,
            editable,
            receives_events,
        }
    }
}
