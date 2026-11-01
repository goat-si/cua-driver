use super::popup_ownership::*;
use std::collections::VecDeque;
use std::time::Duration;

struct Fake {
    menus: VecDeque<MenuState<u32>>,
    last_menu: MenuState<u32>,
    lists: VecDeque<ListRead<u32>>,
    last_list: ListRead<u32>,
    idle: VecDeque<Option<u64>>,
    pending: bool,
    result: PressReturn,
    cancel_result: i32,
    press_result: i32,
    panic_on_options: bool,
    effects: Vec<&'static str>,
    time: Duration,
}

impl Fake {
    fn new(menus: Vec<MenuState<u32>>, list: ListRead<u32>) -> Self {
        Self {
            menus: menus.into(),
            last_menu: MenuState::Unknown,
            lists: VecDeque::new(),
            last_list: list,
            idle: vec![Some(10_000), Some(10_000)].into(),
            pending: true,
            result: PressReturn::Success,
            cancel_result: 0,
            press_result: 0,
            panic_on_options: false,
            effects: vec![],
            time: Duration::ZERO,
        }
    }
}

impl PopupEnv for Fake {
    type Id = u32;
    fn same(&self, a: &u32, b: &u32) -> bool {
        a == b
    }
    fn menu(&mut self) -> MenuState<u32> {
        if let Some(menu) = self.menus.pop_front() {
            self.last_menu = menu;
        }
        self.last_menu.clone()
    }
    fn options(&mut self, _: &u32) -> ListRead<u32> {
        assert!(!self.panic_on_options, "injected option read failure");
        if let Some(list) = self.lists.pop_front() {
            self.last_list = list;
        }
        self.last_list.clone()
    }
    fn idle_ms(&mut self) -> Option<u64> {
        self.idle.pop_front().unwrap_or(Some(10_000))
    }
    fn now(&mut self) -> Duration {
        self.time
    }
    fn pause(&mut self) {
        self.time += Duration::from_millis(50);
    }
    fn open(&mut self) {
        self.effects.push("open");
    }
    fn opener_finished(&mut self) -> bool {
        !self.pending
    }
    fn join(&mut self) -> PressReturn {
        self.effects.push("join");
        self.result
    }
    fn press(&mut self, _: &u32) -> i32 {
        self.effects.push("press");
        self.press_result
    }
    fn cancel(&mut self, _: &u32) -> i32 {
        self.effects.push("cancel");
        self.cancel_result
    }
}

fn list(items: &[(u32, &str, &str)]) -> ListRead<u32> {
    ListRead::Complete(
        items
            .iter()
            .map(|(id, title, value)| OptionEntry {
                id: *id,
                title: (*title).into(),
                value: (*value).into(),
            })
            .collect(),
    )
}

#[test]
fn duplicate_titles_refuse_without_pressing() {
    let mut f = Fake::new(
        vec![MenuState::Open(1)],
        list(&[(2, "Choice", "a"), (3, "Choice", "b")]),
    );
    assert!(matches!(
        select(&mut f, "Choice"),
        PopupOutcome::Refused { .. }
    ));
    assert!(f.effects.is_empty());
}

#[test]
fn a_unique_title_still_selects() {
    let mut f = Fake::new(vec![MenuState::Open(1)], list(&[(2, "Choice", "a")]));
    assert!(matches!(
        select(&mut f, "Choice"),
        PopupOutcome::Selected {
            preexisting: true,
            ..
        }
    ));
    assert_eq!(f.effects, ["press"]);
}

#[test]
fn unknown_initial_menu_refuses_without_mutation() {
    let mut f = Fake::new(vec![MenuState::Unknown], list(&[]));
    assert!(matches!(select(&mut f, "x"), PopupOutcome::Refused { .. }));
    assert!(f.effects.is_empty());
}

#[test]
fn preexisting_empty_menu_residual_is_documented() {
    // AX cannot distinguish a closed menu from a preexisting empty menu.
    // Opening may therefore dismiss the latter; it is never adopted later.
    let mut f = Fake::new(vec![MenuState::ClosedOrEmpty], list(&[]));
    f.pending = false;
    assert!(matches!(
        select(&mut f, "x"),
        PopupOutcome::StoppedAfterOpen {
            close: CloseStatus::Unconfirmed,
            ..
        }
    ));
    assert_eq!(f.effects, ["open", "join"]);
}

#[test]
fn a_preexisting_menu_is_not_closed_by_this_call() {
    let mut f = Fake::new(vec![MenuState::Open(1)], list(&[(2, "Other", "a")]));
    assert!(matches!(
        select(&mut f, "Missing"),
        PopupOutcome::Refused { .. }
    ));
    assert!(f.effects.is_empty());
}

#[test]
fn refusal_on_preexisting_menu_reports_its_ownership() {
    let mut f = Fake::new(vec![MenuState::Open(1)], list(&[(2, "Other", "a")]));
    let outcome = select(&mut f, "Missing");
    let result = tool_result(&outcome, "", false);
    assert!(result.structured_content.unwrap()["note"]
        .as_str()
        .unwrap_or("")
        .contains("previously open"));
    assert!(f.effects.is_empty());
}

#[test]
fn physical_input_after_t0_blocks_ownership() {
    let mut f = Fake::new(
        vec![
            MenuState::ClosedOrEmpty,
            MenuState::ClosedOrEmpty,
            MenuState::Open(1),
        ],
        list(&[(2, "x", "a")]),
    );
    f.idle = vec![Some(10_000), Some(0)].into();
    assert!(matches!(
        select(&mut f, "x"),
        PopupOutcome::StoppedAfterOpen {
            close: CloseStatus::Unconfirmed,
            ..
        }
    ));
    assert_eq!(f.effects, ["open", "join"]);
}

#[test]
fn unknown_input_sample_at_t0_blocks_ownership() {
    let mut f = Fake::new(
        vec![MenuState::ClosedOrEmpty, MenuState::Open(1)],
        list(&[(2, "x", "a")]),
    );
    f.idle = vec![None, Some(100)].into();
    assert!(matches!(
        select(&mut f, "x"),
        PopupOutcome::StoppedAfterOpen {
            close: CloseStatus::Unconfirmed,
            ..
        }
    ));
    assert_eq!(f.effects, ["open", "join"]);
}

#[test]
fn input_at_the_clock_boundary_does_not_prove_ownership() {
    let mut f = Fake::new(
        vec![MenuState::ClosedOrEmpty, MenuState::Open(1)],
        list(&[(2, "x", "a")]),
    );
    f.idle = vec![Some(100), Some(0)].into();
    assert!(matches!(
        select(&mut f, "x"),
        PopupOutcome::StoppedAfterOpen {
            close: CloseStatus::Unconfirmed,
            ..
        }
    ));
    assert_eq!(f.effects, ["open", "join"]);
}

#[test]
fn unknown_input_sample_at_observation_blocks_ownership() {
    let mut f = Fake::new(
        vec![MenuState::ClosedOrEmpty, MenuState::Open(1)],
        list(&[(2, "x", "a")]),
    );
    f.idle = vec![Some(100), None].into();
    assert!(matches!(
        select(&mut f, "x"),
        PopupOutcome::StoppedAfterOpen {
            close: CloseStatus::Unconfirmed,
            ..
        }
    ));
    assert_eq!(f.effects, ["open", "join"]);
}

#[test]
fn both_unknown_samples_block_ownership() {
    let mut f = Fake::new(
        vec![MenuState::ClosedOrEmpty, MenuState::Open(1)],
        list(&[(2, "x", "a")]),
    );
    f.idle = vec![None, None].into();
    assert!(matches!(
        select(&mut f, "x"),
        PopupOutcome::StoppedAfterOpen {
            close: CloseStatus::Unconfirmed,
            ..
        }
    ));
    assert_eq!(f.effects, ["open", "join"]);
}

#[test]
fn a_late_menu_after_failed_open_is_not_adopted() {
    let mut f = Fake::new(
        vec![MenuState::ClosedOrEmpty, MenuState::Open(1)],
        list(&[(2, "x", "a")]),
    );
    f.pending = false;
    f.result = PressReturn::Error(-1);
    assert!(matches!(
        select(&mut f, "x"),
        PopupOutcome::StoppedAfterOpen {
            close: CloseStatus::Unconfirmed,
            ..
        }
    ));
    assert_eq!(f.effects, ["open", "join"]);
}

#[test]
fn menu_shown_while_press_pending_is_owned_and_selectable() {
    let mut f = Fake::new(
        vec![
            MenuState::ClosedOrEmpty,
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::ClosedOrEmpty,
        ],
        list(&[(2, "x", "a")]),
    );
    assert!(matches!(
        select(&mut f, "x"),
        PopupOutcome::Selected {
            preexisting: false,
            close: Some(CloseStatus::Confirmed),
            ..
        }
    ));
    assert_eq!(f.effects, ["open", "press", "cancel", "join"]);
}

#[test]
fn opener_timeout_then_items_gone_is_unconfirmed() {
    let mut f = Fake::new(
        vec![
            MenuState::ClosedOrEmpty,
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::ClosedOrEmpty,
        ],
        list(&[(2, "x", "a")]),
    );
    f.result = PressReturn::Timeout;
    assert!(matches!(
        select(&mut f, "x"),
        PopupOutcome::Selected {
            close: Some(CloseStatus::Unconfirmed),
            ..
        }
    ));
    assert_eq!(f.effects, ["open", "press", "cancel", "join"]);
}

#[test]
fn changed_menu_is_never_cancelled_or_reported_closed() {
    let mut f = Fake::new(
        vec![
            MenuState::ClosedOrEmpty,
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::Open(7),
            MenuState::ClosedOrEmpty,
        ],
        list(&[(2, "x", "a")]),
    );
    assert!(matches!(
        select(&mut f, "x"),
        PopupOutcome::Selected {
            close: Some(CloseStatus::Unconfirmed),
            ..
        }
    ));
    assert_eq!(f.effects, ["open", "press", "join"]);
}

#[test]
fn delayed_duplicate_is_not_pressed() {
    let mut f = Fake::new(vec![MenuState::Open(1)], list(&[(2, "x", "a")]));
    f.lists = vec![
        list(&[(2, "x", "a")]),
        list(&[(2, "x", "a"), (3, "x", "b")]),
        list(&[(2, "x", "a"), (3, "x", "b")]),
    ]
    .into();
    assert!(matches!(select(&mut f, "x"), PopupOutcome::Refused { .. }));
    assert!(f.effects.is_empty());
}

#[test]
fn replacement_item_with_same_title_is_not_pressed() {
    let mut f = Fake::new(vec![MenuState::Open(1)], list(&[(2, "x", "a")]));
    f.lists = vec![
        list(&[(2, "x", "a")]),
        list(&[(2, "x", "a")]),
        list(&[(3, "x", "a")]),
    ]
    .into();
    assert!(matches!(select(&mut f, "x"), PopupOutcome::Refused { .. }));
    assert!(f.effects.is_empty());
}

#[test]
fn incomplete_list_never_selects() {
    let mut f = Fake::new(vec![MenuState::Open(1)], ListRead::Incomplete);
    assert!(matches!(select(&mut f, "x"), PopupOutcome::Refused { .. }));
    assert!(f.effects.is_empty());
}

#[test]
fn cancel_failure_sends_no_key_and_cannot_confirm_close() {
    let mut f = Fake::new(
        vec![
            MenuState::ClosedOrEmpty,
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::ClosedOrEmpty,
        ],
        list(&[(2, "x", "a")]),
    );
    f.cancel_result = -1;
    assert!(matches!(
        select(&mut f, "x"),
        PopupOutcome::Selected {
            close: Some(CloseStatus::Unconfirmed),
            ..
        }
    ));
    assert_eq!(f.effects, ["open", "press", "cancel", "join"]);
}

#[test]
fn a_menu_this_call_opened_is_closed_on_failure() {
    let mut f = Fake::new(
        vec![
            MenuState::ClosedOrEmpty,
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::ClosedOrEmpty,
        ],
        list(&[(2, "other", "a")]),
    );
    assert!(matches!(
        select(&mut f, "missing"),
        PopupOutcome::StoppedAfterOpen {
            close: CloseStatus::Confirmed,
            ..
        }
    ));
    assert_eq!(f.effects, ["open", "cancel", "join"]);
}

#[test]
fn item_press_failure_preserves_delivery_uncertainty_and_cleans_up() {
    let mut f = Fake::new(
        vec![
            MenuState::ClosedOrEmpty,
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::ClosedOrEmpty,
        ],
        list(&[(2, "x", "a")]),
    );
    f.press_result = -1;
    assert!(matches!(
        select(&mut f, "x"),
        PopupOutcome::SentUnverified {
            close: Some(CloseStatus::Confirmed),
            ..
        }
    ));
    assert_eq!(f.effects, ["open", "press", "cancel", "join"]);
}

#[test]
fn owned_menu_is_cleaned_up_if_option_read_panics() {
    let mut f = Fake::new(
        vec![
            MenuState::ClosedOrEmpty,
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::Open(1),
        ],
        list(&[]),
    );
    f.panic_on_options = true;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| select(&mut f, "x")));
    assert!(result.is_err());
    assert_eq!(f.effects, ["open", "cancel", "join"]);
}

#[test]
fn opener_error_then_items_gone_is_unconfirmed() {
    let mut f = Fake::new(
        vec![
            MenuState::ClosedOrEmpty,
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::Open(1),
            MenuState::ClosedOrEmpty,
        ],
        list(&[(2, "x", "a")]),
    );
    f.result = PressReturn::Error(-1);
    assert!(matches!(
        select(&mut f, "x"),
        PopupOutcome::Selected {
            close: Some(CloseStatus::Unconfirmed),
            ..
        }
    ));
}
