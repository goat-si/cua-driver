//! Canonical vocabulary derived only from observed AX evidence.
use cua_driver_contract::Role;

/// `subrole` distinguishes a known absence from an unavailable read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Subrole {
    Known(String),
    Absent,
    Unknown,
}

pub(crate) fn canonical_role(role: Option<&str>, subrole: &Subrole, web: bool) -> Option<Role> {
    use Role::*;
    let role = role?;
    let sub = match subrole {
        Subrole::Known(value) => Some(value.as_str()),
        _ => None,
    };
    let special = match (role, sub) {
        ("AXRow", Some("AXOutlineRow")) => Some(TreeItem),
        ("AXCheckBox", Some("AXSwitch")) => Some(Switch),
        ("AXRadioButton", Some("AXTabButton")) => Some(Tab),
        ("AXWindow", Some("AXDialog" | "AXSystemDialog")) => Some(Dialog),
        ("AXDockItem", Some("AXSeparatorDockItem")) => Some(Separator),
        ("AXGroup", Some(value)) => match value {
            "AXLandmarkBanner" => Some(Banner),
            "AXLandmarkComplementary" => Some(Complementary),
            "AXLandmarkContentInfo" => Some(ContentInfo),
            "AXLandmarkMain" => Some(Main),
            "AXLandmarkNavigation" => Some(Navigation),
            "AXLandmarkSearch" => Some(Search),
            "AXLandmarkForm" => Some(Form),
            "AXLandmarkRegion" => Some(Region),
            "AXApplicationAlert" => Some(Alert),
            "AXApplicationAlertDialog" => Some(AlertDialog),
            "AXApplicationDialog" => Some(Dialog),
            "AXApplicationLog" => Some(Log),
            "AXApplicationMarquee" => Some(Marquee),
            "AXApplicationStatus" => Some(Status),
            "AXApplicationTimer" => Some(Timer),
            "AXDocument" => Some(Document),
            "AXDocumentArticle" => Some(Article),
            "AXDocumentMath" => Some(Math),
            "AXDocumentNote" => Some(Note),
            "AXTabPanel" => Some(TabPanel),
            "AXDefinition" => Some(Definition),
            "AXTerm" => Some(Term),
            "AXUserInterfaceTooltip" => Some(Tooltip),
            "AXWebApplication" => Some(Application),
            _ => None,
        },
        ("AXList", None) if web && *subrole == Subrole::Absent => Some(ListBox),
        _ => None,
    };
    Some(special.unwrap_or(match role {
        "AXApplication" => Application,
        "AXWindow" => Window,
        "AXSheet" => Sheet,
        "AXDrawer" => Drawer,
        "AXPopover" => Popover,
        "AXButton" => Button,
        "AXRadioButton" => RadioButton,
        "AXCheckBox" => Checkbox,
        "AXMenuButton" | "AXPopUpButton" => MenuButton,
        "AXComboBox" => ComboBox,
        "AXTabGroup" => TabList,
        "AXTable" => Table,
        "AXColumn" => Column,
        "AXRow" | "AXListItem" => Row,
        "AXOutlineRow" => TreeItem,
        "AXCell" => Cell,
        "AXOutline" => Outline,
        "AXBrowser" => Browser,
        "AXScrollArea" => ScrollArea,
        "AXScrollBar" => ScrollBar,
        "AXRadioGroup" => RadioGroup,
        "AXList" => List,
        "AXGroup" | "AXSplitGroup" | "AXLayoutArea" => Group,
        "AXImage" => Image,
        "AXHeading" => Heading,
        "AXStaticText" => StaticText,
        "AXTextField" | "AXTextArea" | "AXSecureTextField" | "AXSearchField" => TextField,
        "AXSlider" => Slider,
        "AXIncrementor" => Incrementor,
        "AXProgressIndicator" | "AXBusyIndicator" => ProgressBar,
        "AXLevelIndicator" => LevelIndicator,
        "AXRelevanceIndicator" => RelevanceIndicator,
        "AXToolbar" => Toolbar,
        "AXDisclosureTriangle" => Disclosure,
        "AXMenuBar" | "AXMenu" => Menu,
        "AXMenuBarItem" | "AXMenuItem" => MenuItem,
        "AXSplitter" => Splitter,
        "AXColorWell" => ColorWell,
        "AXTimeField" => TimeField,
        "AXDateField" => DateField,
        "AXHelpTag" => HelpTag,
        "AXMatte" => Matte,
        "AXDockItem" => DockItem,
        "AXRuler" => Ruler,
        "AXRulerMarker" => RulerMarker,
        "AXLayoutItem" => LayoutItem,
        "AXHandle" | "AXGrowArea" => Handle,
        "AXSwitch" => Switch,
        "AXTab" => Tab,
        "AXWebArea" => WebArea,
        "AXLink" => Link,
        "AXSeparator" => Separator,
        _ => Unknown,
    }))
}

#[cfg(test)]
mod role_tests {
    use super::*;
    fn known(value: &str) -> Subrole {
        Subrole::Known(value.into())
    }
    #[test]
    fn native_role_table() {
        for (native, expected) in [
            ("AXApplication", Role::Application),
            ("AXWindow", Role::Window),
            ("AXSheet", Role::Sheet),
            ("AXDrawer", Role::Drawer),
            ("AXPopover", Role::Popover),
            ("AXButton", Role::Button),
            ("AXRadioButton", Role::RadioButton),
            ("AXCheckBox", Role::Checkbox),
            ("AXMenuButton", Role::MenuButton),
            ("AXPopUpButton", Role::MenuButton),
            ("AXComboBox", Role::ComboBox),
            ("AXTabGroup", Role::TabList),
            ("AXTable", Role::Table),
            ("AXColumn", Role::Column),
            ("AXRow", Role::Row),
            ("AXCell", Role::Cell),
            ("AXOutline", Role::Outline),
            ("AXBrowser", Role::Browser),
            ("AXScrollArea", Role::ScrollArea),
            ("AXScrollBar", Role::ScrollBar),
            ("AXRadioGroup", Role::RadioGroup),
            ("AXList", Role::List),
            ("AXGroup", Role::Group),
            ("AXSplitGroup", Role::Group),
            ("AXLayoutArea", Role::Group),
            ("AXImage", Role::Image),
            ("AXHeading", Role::Heading),
            ("AXStaticText", Role::StaticText),
            ("AXTextField", Role::TextField),
            ("AXTextArea", Role::TextField),
            ("AXSlider", Role::Slider),
            ("AXIncrementor", Role::Incrementor),
            ("AXProgressIndicator", Role::ProgressBar),
            ("AXBusyIndicator", Role::ProgressBar),
            ("AXLevelIndicator", Role::LevelIndicator),
            ("AXRelevanceIndicator", Role::RelevanceIndicator),
            ("AXToolbar", Role::Toolbar),
            ("AXDisclosureTriangle", Role::Disclosure),
            ("AXMenuBar", Role::Menu),
            ("AXMenu", Role::Menu),
            ("AXMenuBarItem", Role::MenuItem),
            ("AXMenuItem", Role::MenuItem),
            ("AXSplitter", Role::Splitter),
            ("AXColorWell", Role::ColorWell),
            ("AXTimeField", Role::TimeField),
            ("AXDateField", Role::DateField),
            ("AXHelpTag", Role::HelpTag),
            ("AXMatte", Role::Matte),
            ("AXDockItem", Role::DockItem),
            ("AXRuler", Role::Ruler),
            ("AXRulerMarker", Role::RulerMarker),
            ("AXLayoutItem", Role::LayoutItem),
            ("AXHandle", Role::Handle),
            ("AXGrowArea", Role::Handle),
            ("AXOutlineRow", Role::TreeItem),
            ("AXListItem", Role::Row),
            ("AXSecureTextField", Role::TextField),
            ("AXSearchField", Role::TextField),
            ("AXSwitch", Role::Switch),
            ("AXTab", Role::Tab),
            ("AXWebArea", Role::WebArea),
            ("AXLink", Role::Link),
            ("AXSeparator", Role::Separator),
        ] {
            assert_eq!(
                canonical_role(Some(native), &Subrole::Absent, false),
                Some(expected),
                "{native}"
            );
        }
    }
    #[test]
    fn native_subrole_table_and_locate_consequences() {
        for (r, s, want) in [
            ("AXRow", "AXOutlineRow", Role::TreeItem),
            ("AXRow", "AXTableRow", Role::Row),
            ("AXCheckBox", "AXSwitch", Role::Switch),
            ("AXRadioButton", "AXTabButton", Role::Tab),
            ("AXWindow", "AXDialog", Role::Dialog),
            ("AXDockItem", "AXSeparatorDockItem", Role::Separator),
        ] {
            assert_eq!(canonical_role(Some(r), &known(s), false), Some(want));
        }
        for r in ["AXMenuButton", "AXPopUpButton"] {
            assert_eq!(
                canonical_role(Some(r), &Subrole::Absent, false),
                Some(Role::MenuButton)
            );
        }
    }
    #[test]
    fn web_subrole_table() {
        for (s, want) in [
            ("AXLandmarkBanner", Role::Banner),
            ("AXLandmarkComplementary", Role::Complementary),
            ("AXLandmarkContentInfo", Role::ContentInfo),
            ("AXLandmarkMain", Role::Main),
            ("AXLandmarkNavigation", Role::Navigation),
            ("AXLandmarkSearch", Role::Search),
            ("AXLandmarkForm", Role::Form),
            ("AXLandmarkRegion", Role::Region),
            ("AXApplicationAlert", Role::Alert),
            ("AXApplicationAlertDialog", Role::AlertDialog),
            ("AXApplicationDialog", Role::Dialog),
            ("AXApplicationLog", Role::Log),
            ("AXApplicationMarquee", Role::Marquee),
            ("AXApplicationStatus", Role::Status),
            ("AXApplicationTimer", Role::Timer),
            ("AXDocument", Role::Document),
            ("AXDocumentArticle", Role::Article),
            ("AXDocumentMath", Role::Math),
            ("AXDocumentNote", Role::Note),
            ("AXTabPanel", Role::TabPanel),
            ("AXDefinition", Role::Definition),
            ("AXTerm", Role::Term),
            ("AXUserInterfaceTooltip", Role::Tooltip),
            ("AXWebApplication", Role::Application),
        ] {
            assert_eq!(
                canonical_role(Some("AXGroup"), &known(s), true),
                Some(want),
                "{s}"
            );
        }
    }
    #[test]
    fn failed_role_differs_from_successful_unknown() {
        assert_eq!(canonical_role(None, &Subrole::Unknown, false), None);
        for r in [
            "AXUnknown",
            "AXSystemWide",
            "AXValueIndicator",
            "AXNewThing",
        ] {
            assert_eq!(
                canonical_role(Some(r), &Subrole::Unknown, false),
                Some(Role::Unknown)
            );
        }
    }
    #[test]
    fn subrole_failure_never_guesses_listbox() {
        assert_eq!(
            canonical_role(Some("AXList"), &Subrole::Unknown, true),
            Some(Role::List)
        );
        assert_eq!(
            canonical_role(Some("AXList"), &Subrole::Absent, true),
            Some(Role::ListBox)
        );
    }
    #[test]
    fn wrong_combinations_keep_base() {
        assert_eq!(
            canonical_role(Some("AXButton"), &known("AXOutlineRow"), false),
            Some(Role::Button)
        );
        assert_eq!(
            canonical_role(Some("AXGroup"), &known("AXSwitch"), true),
            Some(Role::Group)
        );
    }
    #[test]
    fn ambiguous_web_roles_stay_native() {
        for (r, want) in [
            ("AXTable", Role::Table),
            ("AXStaticText", Role::StaticText),
            ("AXGroup", Role::Group),
            ("AXList", Role::List),
        ] {
            assert_eq!(
                canonical_role(Some(r), &known("AXUnrecognized"), true),
                Some(want)
            );
        }
    }
    #[test]
    fn mapper_preserves_native_input() {
        let native = String::from("AXRow");
        let _ = canonical_role(Some(&native), &known("AXOutlineRow"), false);
        assert_eq!(native, "AXRow");
    }
}
