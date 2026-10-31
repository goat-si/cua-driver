use super::Intent;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Affordance {
    Press,
    Open,
    ShowMenu,
    SetValue,
    Focus,
    Expand,
    Collapse,
    Select,
    Toggle,
    Scroll,
    ScrollIntoView,
    PressKey,
    KeyDown,
    KeyUp,
    TypeText,
    Hover,
    Drag,
}

pub fn supported_affordances(intent: Intent) -> &'static [Affordance] {
    use Affordance::*;
    match intent {
        Intent::Click | Intent::DoubleClick | Intent::TripleClick => &[Press, Open],
        Intent::RightClick => &[ShowMenu],
        Intent::SetValue | Intent::Clear => &[SetValue],
        Intent::TypeText => &[TypeText],
        Intent::Focus => &[Focus],
        Intent::Expand => &[Expand],
        Intent::Collapse => &[Collapse],
        Intent::Select => &[Select, Press, Open],
        Intent::Toggle | Intent::Check | Intent::Uncheck => &[Toggle, Press, Open],
        Intent::Scroll => &[Scroll],
        Intent::ScrollTo => &[ScrollIntoView],
        Intent::PressKey => &[PressKey],
        Intent::KeyDown => &[KeyDown],
        Intent::KeyUp => &[KeyUp],
        Intent::Hover => &[Hover],
        Intent::Drag => &[Drag],
    }
}

pub fn is_primary(affordance: Affordance) -> bool {
    !matches!(
        affordance,
        Affordance::Focus | Affordance::ShowMenu | Affordance::ScrollIntoView
    )
}
