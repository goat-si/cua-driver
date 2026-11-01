use super::*;
use std::cell::Cell;

struct FakeEnv {
    available: bool,
    focus: Cell<u32>,
    belief: bool,
    records: Cell<usize>,
    ax_writes: Cell<usize>,
    key_posts: Cell<usize>,
}

impl FakeEnv {
    fn new(available: bool, focus: u32, belief: bool) -> Self {
        Self {
            available,
            focus: Cell::new(focus),
            belief,
            records: Cell::new(0),
            ax_writes: Cell::new(0),
            key_posts: Cell::new(0),
        }
    }
}

impl KeyEnv for FakeEnv {
    fn post_spi_available(&self) -> bool {
        self.available
    }
    fn key_focus(&self, _: pid_t) -> KeyFocus {
        KeyFocus {
            window: Some(self.focus.get()),
            element_window: Some(self.focus.get()),
        }
    }
    fn window_order(&self, _: pid_t) -> Vec<u32> {
        vec![1, 2]
    }
    fn post_records(
        &self,
        _: pid_t,
        _: Option<u32>,
        target: u32,
        _: RecordStyle,
    ) -> Option<RecordPosts> {
        self.records.set(self.records.get() + 1);
        self.focus.set(target);
        Some(RecordPosts {
            posted: 4,
            failed: 0,
        })
    }
    fn await_focus(&self, _: pid_t, _: u32, _: std::time::Duration, _: FocusPollMode) {}
    fn needs_full_records(&self, _: pid_t) -> bool {
        false
    }
    fn note_needs_full_records(&self, _: pid_t) {}
    fn set_focused_window(&self, _: pid_t, wid: u32) -> bool {
        self.ax_writes.set(self.ax_writes.get() + 1);
        self.focus.set(wid);
        true
    }
    fn belief_match(&self, _: pid_t, _: u32) -> BeliefMatch {
        if self.belief {
            BeliefMatch::FreshKeyAndActive
        } else {
            BeliefMatch::Stale
        }
    }
    fn belief_ticket(&self, _: pid_t) -> Option<Ticket> {
        None
    }
    fn belief_now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }
    fn prepare_belief(&self, _: Ticket, _: u32, _: KeyFocus) -> bool {
        false
    }
    fn publish_belief(&self, _: Ticket, _: u32, _: KeyFocus, _: std::time::Instant) {}
}

fn exercise(env: &FakeEnv, eligibility: FastPathEligibility) -> Result<(), ScopeEntryError> {
    let ops = ScopeOps {
        window_order: |_| vec![1, 2],
        hand_back: |_, _, _, _| HandBack::Restored,
    };
    let mut scope = enter_scope_with(env, ops, -99_124, 2, eligibility)?;
    env.key_posts.set(env.key_posts.get() + 1);
    scope.done = true;
    Ok(())
}

#[test]
fn missing_post_spi_refuses_before_records_or_ax_focus_write() {
    let _serial = key_scope_tests_serial();
    let env = FakeEnv::new(false, 1, false);
    let result = exercise(&env, FastPathEligibility::Ineligible);
    assert!(result.is_err());
    assert_eq!(
        (env.key_posts.get(), env.records.get(), env.ax_writes.get()),
        (0, 0, 0)
    );
}

#[test]
fn missing_post_spi_refuses_already_key_and_belief() {
    let _serial = key_scope_tests_serial();
    for belief in [false, true] {
        let env = FakeEnv::new(false, 2, belief);
        let result = exercise(&env, FastPathEligibility::Eligible);
        assert!(result.is_err(), "belief={belief}");
        assert_eq!(
            (env.key_posts.get(), env.records.get(), env.ax_writes.get()),
            (0, 0, 0)
        );
    }
}

#[test]
fn available_post_spi_keeps_existing_entry() {
    let _serial = key_scope_tests_serial();
    let env = FakeEnv::new(true, 1, false);
    assert!(exercise(&env, FastPathEligibility::Ineligible).is_ok());
    assert_eq!(
        (env.key_posts.get(), env.records.get(), env.ax_writes.get()),
        (1, 1, 0)
    );
}
