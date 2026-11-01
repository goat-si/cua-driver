use super::popup_ownership::{self, *};
use std::time::Duration;

struct Fake {
    menu: Option<u8>,
    opened: bool,
    replace_after_press: bool,
    cancel_code: i32,
    press_code: i32,
    cancel_panics: bool,
    opener: PressReturn,
    presses: usize,
    cancels: usize,
    joins: usize,
    ticks: u64,
}

impl Fake {
    fn owned() -> Self {
        Self {
            menu: None,
            opened: false,
            replace_after_press: false,
            cancel_code: 0,
            press_code: crate::ax::bindings::kAXErrorCannotComplete,
            cancel_panics: false,
            opener: PressReturn::Success,
            presses: 0,
            cancels: 0,
            joins: 0,
            ticks: 0,
        }
    }
}

impl PopupEnv for Fake {
    type Id = u8;
    fn same(&self, a: &u8, b: &u8) -> bool {
        a == b
    }
    fn menu(&mut self) -> MenuState<u8> {
        self.menu.map_or(MenuState::ClosedOrEmpty, MenuState::Open)
    }
    fn options(&mut self, _: &u8) -> ListRead<u8> {
        ListRead::Complete(vec![OptionEntry {
            id: 2,
            title: "choice".into(),
            value: String::new(),
        }])
    }
    fn idle_ms(&mut self) -> Option<u64> {
        Some(10_000)
    }
    fn now(&mut self) -> Duration {
        Duration::from_millis(self.ticks)
    }
    fn pause(&mut self) {
        self.ticks += 50;
    }
    fn open(&mut self) {
        self.opened = true;
        self.menu = Some(1);
    }
    fn opener_finished(&mut self) -> bool {
        false
    }
    fn join(&mut self) -> PressReturn {
        self.joins += 1;
        self.opener
    }
    fn press(&mut self, _: &u8) -> i32 {
        assert_eq!(
            self.joins, 0,
            "pending opener was joined before option dispatch"
        );
        self.presses += 1;
        if self.replace_after_press {
            self.menu = Some(3);
        }
        self.press_code
    }
    fn cancel(&mut self, _: &u8) -> i32 {
        self.cancels += 1;
        assert!(!self.cancel_panics, "injected cancellation panic");
        if self.cancel_code == 0 {
            self.menu = None;
        }
        self.cancel_code
    }
}

#[test]
fn popup_error_never_claims_no_input_after_opener() {
    let mut preexisting = Fake::owned();
    preexisting.menu = Some(1);
    let _ = popup_ownership::select(&mut preexisting, "choice");
    assert_eq!(
        (preexisting.presses, preexisting.cancels, preexisting.joins),
        (1, 0, 0)
    );

    let mut replaced = Fake::owned();
    replaced.replace_after_press = true;
    let _ = popup_ownership::select(&mut replaced, "choice");
    assert_eq!(
        (replaced.presses, replaced.cancels, replaced.joins),
        (1, 0, 1)
    );

    let mut failed_close = Fake::owned();
    failed_close.cancel_code = crate::ax::bindings::kAXErrorFailure;
    let _ = popup_ownership::select(&mut failed_close, "choice");
    assert_eq!((failed_close.presses, failed_close.cancels), (1, 1));

    let mut panic_close = Fake::owned();
    panic_close.cancel_panics = true;
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        popup_ownership::select(&mut panic_close, "choice")
    }));
    assert!(panic.is_err());
    assert_eq!((panic_close.presses, panic_close.cancels), (1, 1));

    let mut pending = Fake::owned();
    pending.opener = PressReturn::Error(crate::ax::bindings::kAXErrorFailure);
    let pending_outcome = popup_ownership::select(&mut pending, "choice");
    assert!(matches!(
        pending_outcome,
        PopupOutcome::SentUnverified {
            close: Some(CloseStatus::Unconfirmed),
            ..
        }
    ));
    assert_eq!((pending.presses, pending.cancels, pending.joins), (1, 1, 1));

    let mut fake = Fake::owned();
    let outcome = popup_ownership::select(&mut fake, "choice");
    assert_eq!((fake.presses, fake.cancels, fake.joins), (1, 1, 1));
    let result = popup_ownership::tool_result(&outcome, "", false);
    assert_eq!(
        result.disposition,
        Some(cua_driver_contract::ErrorDisposition::SUSPECTED_NOOP)
    );
    let normalized = cua_driver_core::mcp_result::finalize_error_disposition(
        "set_value",
        serde_json::to_value(result).unwrap(),
    );
    assert_eq!(normalized["structuredContent"]["effect"], "unverifiable");
    assert_eq!(
        normalized["structuredContent"]["disposition"]["delivery"],
        "delivery_uncertain"
    );
    assert_eq!(
        normalized["structuredContent"]["disposition"]["retry"],
        "unsafe"
    );
}

#[test]
fn popup_refusal_after_prior_typeahead_is_not_clean_refusal() {
    let mut fake = Fake::owned();
    fake.menu = Some(1);
    let outcome = super::popup_after_prior(popup_ownership::select(&mut fake, "absent"), true);
    let result = popup_ownership::tool_result(&outcome, "", false);
    assert_ne!(
        result.disposition,
        Some(cua_driver_contract::ErrorDisposition::NOT_DELIVERED)
    );
    assert_eq!(result.structured_content.unwrap()["effect"], "unverifiable");
    assert_eq!((fake.presses, fake.cancels), (0, 0));

    let mut prior = Fake::owned();
    prior.menu = Some(1);
    prior.press_code = crate::ax::bindings::kAXErrorAPIDisabled;
    let outcome = popup_ownership::select_with_prior(
        &mut prior,
        "choice",
        cua_driver_core::mutation::PriorInput::MayHaveDelivered,
    );
    let result = popup_ownership::tool_result(&outcome, "", false);
    assert_eq!(
        result.disposition,
        Some(cua_driver_contract::ErrorDisposition::SUSPECTED_NOOP)
    );
    assert_eq!((prior.presses, prior.cancels), (1, 0));
    for error in [
        crate::ax::bindings::kAXErrorCannotComplete,
        crate::ax::bindings::kAXErrorFailure,
    ] {
        let decision = super::popup_typeahead_input(
            super::Settable::Yes,
            || error,
            || panic!("read after uncertain focus"),
            || panic!("key after uncertain focus"),
        )
        .unwrap();
        assert!(matches!(decision, super::TypeaheadResult::SentUnverified));
        let output =
            super::popup_route(decision, |_| panic!("fallback after uncertain focus")).unwrap();
        assert!(output.contains("verify before repeating"));
    }
    let decision = super::popup_typeahead_input(
        super::Settable::Yes,
        || crate::ax::bindings::kAXErrorSuccess,
        || true,
        || false,
    )
    .unwrap();
    assert!(matches!(decision, super::TypeaheadResult::SentUnverified));
    let _ = super::popup_route(decision, |_| panic!("fallback after uncertain key"));
}
