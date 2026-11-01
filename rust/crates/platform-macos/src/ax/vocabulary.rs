//! Canonical vocabulary derived only from observed AX evidence.
use cua_driver_contract::Role;

/// `subrole` distinguishes a known absence from an unavailable read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Subrole {
    Known(String),
    Absent,
    Unknown,
}

#[derive(Clone, Debug)]
pub(crate) struct CanonicalEvidence {
    pub role_read: bool,
    pub subrole: Subrole,
    pub secure: Option<bool>,
    pub copied_value: Option<String>,
    // Retained for the later completeness consumer.
    #[allow(dead_code)]
    pub classification_failures: [Option<super::content_attrs::AttrStatus>; 2],
    pub value_settable_observed_at: Option<std::time::Instant>,
    pub states: StateEvidence,
}

impl Default for CanonicalEvidence {
    fn default() -> Self {
        Self {
            role_read: false,
            subrole: Subrole::Unknown,
            secure: None,
            copied_value: None,
            classification_failures: [None, None],
            value_settable_observed_at: None,
            states: StateEvidence::default(),
        }
    }
}

impl From<&super::content_attrs::AttrStatus> for Subrole {
    fn from(status: &super::content_attrs::AttrStatus) -> Self {
        use super::content_attrs::AttrStatus;
        match status {
            AttrStatus::Known(value) => Self::Known(value.clone()),
            AttrStatus::Absent => Self::Absent,
            AttrStatus::Failed { .. } | AttrStatus::WrongType => Self::Unknown,
        }
    }
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

#[derive(Clone, Debug)]
pub(crate) enum Observation<T> {
    Known(T),
    Absent,
    // The later completeness consumer needs the native batch failure.
    #[allow(dead_code)]
    Failed(super::batch_attrs::BatchFailure),
    WrongType,
    NotRead,
    Expired,
}

#[derive(Clone, Debug)]
pub(crate) enum StateValue {
    Bool(bool),
    Text(String),
}

#[derive(Clone, Debug)]
pub(crate) struct StateEvidence {
    pub attributes: [Observation<StateValue>; 8],
    pub value_settable: Observation<super::write_outcome::Settable>,
}
impl Default for StateEvidence {
    fn default() -> Self {
        Self {
            attributes: std::array::from_fn(|_| Observation::NotRead),
            value_settable: Observation::NotRead,
        }
    }
}

pub(crate) fn canonical_states(
    role: &str,
    evidence: &CanonicalEvidence,
    enabled: Option<bool>,
    selected: Option<bool>,
    focused: bool,
) -> Option<Vec<cua_driver_contract::State>> {
    use cua_driver_contract::State;
    use Observation::Known;
    let mut result = Vec::new();
    let mut observed = false;
    let mut add = |state, value: Option<bool>| {
        if let Some(value) = value {
            observed = true;
            if value {
                result.push(state);
            }
        }
    };
    if focused {
        add(State::Focused, Some(true));
    }
    add(State::Disabled, enabled.map(|v| !v));
    add(State::Secure, evidence.secure);
    let bool_attr = |index| match &evidence.states.attributes[index] {
        Known(StateValue::Bool(value)) => Some(*value),
        _ => None,
    };
    add(State::Expanded, bool_attr(0));
    let sub = match &evidence.subrole {
        Subrole::Known(s) => Some(s.as_str()),
        _ => None,
    };
    let toggle = match evidence.copied_value.as_deref().map(str::trim) {
        Some("1" | "true" | "on") => Some(1),
        Some("0" | "false" | "off") => Some(0),
        Some("2" | "mixed") => Some(2),
        _ => None,
    };
    let check_role = matches!(role, "AXCheckBox" | "AXSwitch" | "AXRadioButton");
    let button_toggle = role == "AXButton" && sub == Some("AXToggle");
    let checked = if check_role {
        toggle.and_then(|v| match v {
            0 => Some(false),
            1 => Some(true),
            2 if role == "AXCheckBox" && sub != Some("AXSwitch") => Some(false),
            _ => None,
        })
    } else {
        None
    };
    add(State::Checked, checked);
    add(State::Selected, selected);
    add(State::Busy, bool_attr(4));
    add(State::Modal, bool_attr(5));
    add(State::Required, bool_attr(1));
    let mixed = if (role == "AXCheckBox" && sub != Some("AXSwitch")) || button_toggle {
        toggle.map(|v| v == 2)
    } else {
        None
    };
    add(State::Indeterminate, mixed);
    let pressed = if button_toggle {
        toggle.and_then(|v| match v {
            0 => Some(false),
            1 => Some(true),
            2 => Some(false),
            _ => None,
        })
    } else {
        None
    };
    add(State::Pressed, pressed);
    let editable = bool_attr(3);
    let text_input = matches!(
        role,
        "AXTextField" | "AXTextArea" | "AXSecureTextField" | "AXSearchField"
    );
    let readonly = match (text_input, &evidence.states.value_settable, editable) {
        (true, Known(super::write_outcome::Settable::No), Some(true)) => None,
        (true, Known(super::write_outcome::Settable::No), _) => Some(true),
        (true, Known(super::write_outcome::Settable::Yes), _) => Some(false),
        _ => None,
    };
    add(State::Readonly, readonly);
    let invalid = match &evidence.states.attributes[2] {
        Known(StateValue::Bool(value)) => Some(*value),
        Known(StateValue::Text(value)) => match value.as_str() {
            "true" | "spelling" | "grammar" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
    };
    add(State::Invalid, invalid);
    add(State::Multiselectable, bool_attr(6));
    let popup = match &evidence.states.attributes[7] {
        Known(StateValue::Bool(value)) => Some(*value),
        Known(StateValue::Text(value)) => match value.as_str() {
            "true" | "menu" | "listbox" | "tree" | "grid" | "dialog" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
    };
    add(State::Haspopup, popup);
    observed.then_some(result)
}

const STATE_ATTRIBUTES: [&str; 8] = [
    "AXExpanded",
    "AXRequired",
    "AXInvalid",
    "AXEditable",
    "AXElementBusy",
    "AXModal",
    "AXMultiSelectable",
    "AXPopupValue",
];

fn discard_late_settable(evidence: &mut CanonicalEvidence, deadline: std::time::Instant) {
    if evidence
        .value_settable_observed_at
        .is_some_and(|at| at >= deadline)
    {
        evidence.states.value_settable = Observation::Expired;
    }
}

/// One batch and at most one previously unperformed settable query. The clock
/// is injected so tests can prove late replies are discarded without sleeping.
pub(crate) fn collect_state_evidence<S: super::tree_source::AxTreeSource>(
    source: &S,
    element: &S::Element,
    role: &str,
    evidence: &mut CanonicalEvidence,
    deadline: std::time::Instant,
    mut now: impl FnMut() -> std::time::Instant,
) {
    use super::content_attrs::AttrStatus;
    use super::tree_source::{Read, TreeBatch};
    use Observation::*;
    discard_late_settable(evidence, deadline);
    let started = now();
    if started >= deadline {
        evidence.states.attributes = std::array::from_fn(|_| Expired);
        if matches!(evidence.states.value_settable, NotRead) {
            evidence.states.value_settable = Expired;
        }
        return;
    }
    source.set_optional_timeout(element, (deadline - started).as_secs_f32().min(0.25));
    if now() >= deadline {
        evidence.states.attributes = std::array::from_fn(|_| Expired);
        if matches!(evidence.states.value_settable, NotRead) {
            evidence.states.value_settable = Expired;
        }
        source.set_optional_timeout(element, 0.0);
        return;
    }
    let batch = source.batch(element, &STATE_ATTRIBUTES);
    if now() >= deadline {
        evidence.states.attributes = std::array::from_fn(|_| Expired);
        if matches!(evidence.states.value_settable, NotRead) {
            evidence.states.value_settable = Expired;
        }
        source.set_optional_timeout(element, 0.0);
        return;
    }
    evidence.states.attributes = match batch {
        Err(error) => std::array::from_fn(|_| Failed(error)),
        Ok(batch) => std::array::from_fn(|i| match batch.number(i) {
            Read::Known(0.0) => Known(StateValue::Bool(false)),
            Read::Known(1.0) => Known(StateValue::Bool(true)),
            Read::Known(_) => WrongType,
            Read::Absent => Absent,
            Read::Unknown(_) => match batch.boolean(i) {
                Read::Known(v) => Known(StateValue::Bool(v)),
                Read::Absent => Absent,
                Read::Unknown(_) if i == 2 || i == 7 => match batch.text(i) {
                    AttrStatus::Known(v) => Known(StateValue::Text(v)),
                    AttrStatus::Absent => Absent,
                    AttrStatus::Failed { .. } | AttrStatus::WrongType => WrongType,
                },
                Read::Unknown(_) => WrongType,
            },
        }),
    };
    let text_input = matches!(
        role,
        "AXTextField" | "AXTextArea" | "AXSecureTextField" | "AXSearchField"
    );
    if text_input && matches!(evidence.states.value_settable, NotRead) {
        let before = now();
        if before >= deadline {
            evidence.states.value_settable = Expired;
            source.set_optional_timeout(element, 0.0);
            return;
        }
        source.set_optional_timeout(element, (deadline - before).as_secs_f32().min(0.25));
        if now() >= deadline {
            evidence.states.value_settable = Expired;
            source.set_optional_timeout(element, 0.0);
            return;
        }
        let result = source.value_settable(element);
        evidence.states.value_settable = if now() >= deadline {
            Expired
        } else {
            Known(result)
        };
    }
    source.set_optional_timeout(element, 0.0);
}

/// Apply the post-walk read to the same retained node that is serialized.
pub(crate) fn collect_node_state_evidence<S: super::tree_source::AxTreeSource>(
    source: &S,
    element: &S::Element,
    node: &mut super::tree::AXNode,
    deadline: std::time::Instant,
    now: impl FnMut() -> std::time::Instant,
) {
    if node.element_index.is_some() {
        collect_state_evidence(
            source,
            element,
            &node.role,
            &mut node.canonical,
            deadline,
            now,
        );
    }
}

/// The walker retained each indexed pointer for the snapshot; borrow an extra
/// retain only for this bounded read and release it on return.
pub(crate) fn collect_native_state_evidence(
    nodes: &mut [super::tree::AXNode],
    deadline: std::time::Instant,
) {
    use super::tree_source::native::NativeSource;
    for node in nodes
        .iter_mut()
        .filter(|node| node.element_index.is_some() && node.element_ptr != 0)
    {
        discard_late_settable(&mut node.canonical, deadline);
        if std::time::Instant::now() >= deadline {
            node.canonical.states.attributes = std::array::from_fn(|_| Observation::Expired);
            if matches!(node.canonical.states.value_settable, Observation::NotRead) {
                node.canonical.states.value_settable = Observation::Expired;
            }
            continue;
        }
        let element =
            unsafe { super::tree_source::NativeElement::from_borrowed(node.element_ptr as _) };
        collect_node_state_evidence(
            &NativeSource,
            &element,
            node,
            deadline,
            std::time::Instant::now,
        );
    }
}

#[cfg(test)]
mod state_tests {
    use super::super::write_outcome::Settable;
    use super::*;
    use cua_driver_contract::State::*;
    use Observation::Known;
    fn evidence() -> CanonicalEvidence {
        CanonicalEvidence::default()
    }
    fn states(
        role: &str,
        evidence: &CanonicalEvidence,
        enabled: Option<bool>,
        selected: Option<bool>,
        focused: bool,
    ) -> Option<Vec<cua_driver_contract::State>> {
        canonical_states(role, evidence, enabled, selected, focused)
    }
    fn attr(e: &mut CanonicalEvidence, n: usize, v: StateValue) {
        e.states.attributes[n] = Known(v);
    }
    #[test]
    fn enabled_three_way() {
        let e = evidence();
        assert_eq!(states("AXButton", &e, None, None, false), None);
        assert_eq!(
            states("AXButton", &e, Some(true), None, false),
            Some(vec![])
        );
        assert_eq!(
            states("AXButton", &e, Some(false), None, false),
            Some(vec![Disabled])
        );
    }
    #[test]
    fn focused_is_positive_only() {
        assert_eq!(
            states("AXButton", &evidence(), None, None, true),
            Some(vec![Focused])
        );
    }
    #[test]
    fn final_selected_observation() {
        for (value, want) in [(true, vec![Selected]), (false, vec![])] {
            assert_eq!(
                states("AXButton", &evidence(), None, Some(value), false),
                Some(want)
            );
        }
    }
    #[test]
    fn secure_uses_classification() {
        let mut e = evidence();
        e.secure = Some(true);
        assert_eq!(
            states("AXTextField", &e, None, None, false),
            Some(vec![Secure])
        );
    }
    #[test]
    fn checkbox_and_switch_values() {
        let mut e = evidence();
        for r in ["AXCheckBox", "AXSwitch"] {
            for (v, expected) in [
                ("1", true),
                ("true", true),
                ("on", true),
                ("0", false),
                ("false", false),
                ("off", false),
            ] {
                e.copied_value = Some(v.into());
                let got = states(r, &e, None, None, false).unwrap();
                assert_eq!(got.contains(&Checked), expected);
            }
        }
    }
    #[test]
    fn radio_mixed_is_unknown() {
        let mut e = evidence();
        e.copied_value = Some("mixed".into());
        assert_eq!(states("AXRadioButton", &e, None, None, false), None);
        e.copied_value = Some("1".into());
        assert_eq!(
            states("AXRadioButton", &e, None, None, false),
            Some(vec![Checked])
        );
    }
    #[test]
    fn mixed_only_checkbox_and_toggle() {
        let mut e = evidence();
        e.copied_value = Some("2".into());
        assert_eq!(
            states("AXCheckBox", &e, None, None, false),
            Some(vec![Indeterminate])
        );
        assert_eq!(states("AXSwitch", &e, None, None, false), None);
        e.subrole = Subrole::Known("AXToggle".into());
        assert_eq!(
            states("AXButton", &e, None, None, false),
            Some(vec![Indeterminate])
        );
    }
    #[test]
    fn pressed_only_toggle_button() {
        let mut e = evidence();
        e.copied_value = Some("1".into());
        assert_eq!(states("AXButton", &e, None, None, false), None);
        e.subrole = Subrole::Known("AXToggle".into());
        assert_eq!(
            states("AXButton", &e, None, None, false),
            Some(vec![Pressed])
        );
    }
    #[test]
    fn required_and_invalid_values() {
        let mut e = evidence();
        attr(&mut e, 1, StateValue::Bool(true));
        for v in ["true", "spelling", "grammar"] {
            attr(&mut e, 2, StateValue::Text(v.into()));
            assert_eq!(
                states("AXButton", &e, None, None, false),
                Some(vec![Required, Invalid])
            );
        }
        attr(&mut e, 2, StateValue::Text("false".into()));
        assert_eq!(
            states("AXButton", &e, None, None, false),
            Some(vec![Required])
        );
    }
    #[test]
    fn readonly_requires_settable_and_no_conflict() {
        let mut e = evidence();
        e.states.value_settable = Known(Settable::No);
        assert_eq!(
            states("AXTextField", &e, None, None, false),
            Some(vec![Readonly])
        );
        assert_eq!(states("AXCheckBox", &e, None, None, false), None);
        attr(&mut e, 3, StateValue::Bool(true));
        assert_eq!(states("AXTextField", &e, None, None, false), None);
        e.states.value_settable = Known(Settable::Unknown(-1));
        assert_eq!(states("AXTextField", &e, None, None, false), None);
    }
    #[test]
    fn other_attributes_and_unsupported() {
        let mut e = evidence();
        for i in [0, 4, 5, 6] {
            attr(&mut e, i, StateValue::Bool(true));
        }
        attr(&mut e, 7, StateValue::Text("menu".into()));
        assert_eq!(
            states("AXButton", &e, None, None, false),
            Some(vec![Expanded, Busy, Modal, Multiselectable, Haspopup])
        );
    }
    #[test]
    fn order_duplicates_and_empty() {
        let mut e = evidence();
        e.secure = Some(true);
        e.copied_value = Some("1".into());
        e.subrole = Subrole::Known("AXSwitch".into());
        attr(&mut e, 0, StateValue::Bool(true));
        attr(&mut e, 1, StateValue::Bool(true));
        assert_eq!(
            states("AXCheckBox", &e, Some(false), Some(true), true),
            Some(vec![
                Focused, Disabled, Secure, Expanded, Checked, Selected, Required
            ])
        );
        let mut e = evidence();
        attr(&mut e, 0, StateValue::Bool(false));
        assert_eq!(states("AXButton", &e, None, None, false), Some(vec![]));
    }
}
