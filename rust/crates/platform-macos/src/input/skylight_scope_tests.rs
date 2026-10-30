use super::*;
use std::cell::RefCell;

#[test]
fn fast_confirmation_budget_is_forty_milliseconds() {
    assert_eq!(FAST_CONFIRM, std::time::Duration::from_millis(40));
}

#[test]
fn fast_confirmation_polls_every_two_ms_and_stops_at_forty_ms() {
    use std::cell::{Cell, RefCell};
    use std::time::{Duration, Instant};
    let start = Instant::now();
    let elapsed = Cell::new(Duration::ZERO);
    let sleeps = RefCell::new(Vec::new());
    await_key_focus_with(
        Duration::from_millis(40),
        || false,
        || start + elapsed.get(),
        |duration| {
            sleeps.borrow_mut().push(duration);
            elapsed.set(elapsed.get() + duration);
        },
    );
    assert_eq!(elapsed.get(), Duration::from_millis(40));
    assert_eq!(*sleeps.borrow(), vec![Duration::from_millis(2); 20]);
}

#[test]
fn fast_confirmation_stops_as_soon_as_focus_settles() {
    use std::cell::Cell;
    use std::time::{Duration, Instant};
    let start = Instant::now();
    let elapsed = Cell::new(Duration::ZERO);
    await_key_focus_with(
        Duration::from_millis(40),
        || elapsed.get() >= Duration::from_millis(6),
        || start + elapsed.get(),
        |duration| elapsed.set(elapsed.get() + duration),
    );
    assert_eq!(elapsed.get(), Duration::from_millis(6));
}

#[derive(Default)]
struct State {
    window: Option<u32>,
    element_window: Option<u32>,
    key_focus_reads: usize,
    waits: Vec<std::time::Duration>,
    posts: Vec<(Option<u32>, u32, RecordStyle)>,
    ax_writes: Vec<u32>,
    order_reads: usize,
}

/// An app whose AX focus follows records and focused-window writes when
/// `obeys` is set, and ignores them otherwise. Which app is
/// frontmost is not something entering or handing back can read, so the
/// fake has no such field: the tests below hold for either answer.
struct FakeEnv {
    /// The app takes the short record form.
    obeys_fast: bool,
    /// Apps remembered as needing the full sequence.
    needs_full: RefCell<Vec<pid_t>>,
    /// The app takes the full record sequence.
    obeys_records: bool,
    obeys_ax: bool,
    incarnation: std::cell::Cell<Option<(u64, u64)>>,
    notification_generation: std::cell::Cell<u64>,
    notify_during_focus_read: std::cell::Cell<usize>,
    mutation_during_focus_read: std::cell::Cell<usize>,
    observer_registered: std::cell::Cell<bool>,
    standard_window: std::cell::Cell<bool>,
    app_active: std::cell::Cell<bool>,
    activation_posts: std::cell::Cell<usize>,
    now: std::cell::Cell<std::time::Instant>,
    focus_epoch: std::cell::Cell<u64>,
    seen_notification: std::cell::Cell<u64>,
    beliefs: super::super::key_belief::BeliefStore,
    state: RefCell<State>,
}

impl FakeEnv {
    fn new(key_window: u32) -> Self {
        Self {
            obeys_fast: true,
            needs_full: RefCell::new(Vec::new()),
            obeys_records: true,
            obeys_ax: true,
            incarnation: std::cell::Cell::new(Some((1, 1))),
            notification_generation: std::cell::Cell::new(0),
            notify_during_focus_read: std::cell::Cell::new(0),
            mutation_during_focus_read: std::cell::Cell::new(0),
            observer_registered: std::cell::Cell::new(true),
            standard_window: std::cell::Cell::new(true),
            app_active: std::cell::Cell::new(true),
            activation_posts: std::cell::Cell::new(0),
            now: std::cell::Cell::new(std::time::Instant::now()),
            focus_epoch: std::cell::Cell::new(0),
            seen_notification: std::cell::Cell::new(0),
            beliefs: super::super::key_belief::BeliefStore::default(),
            state: RefCell::new(State {
                window: Some(key_window),
                element_window: Some(key_window),
                ..State::default()
            }),
        }
    }
}

impl KeyEnv for FakeEnv {
    fn key_focus(&self, _: pid_t) -> KeyFocus {
        if self.notify_during_focus_read.get() > 0 {
            self.notify_during_focus_read
                .set(self.notify_during_focus_read.get() - 1);
            if self.notify_during_focus_read.get() == 0 {
                self.notification_generation
                    .set(self.notification_generation.get() + 1);
            }
        }
        if self.mutation_during_focus_read.get() > 0 {
            self.mutation_during_focus_read
                .set(self.mutation_during_focus_read.get() - 1);
            if self.mutation_during_focus_read.get() == 0 {
                self.focus_epoch.set(self.focus_epoch.get() + 1);
            }
        }
        let mut state = self.state.borrow_mut();
        state.key_focus_reads += 1;
        KeyFocus {
            window: state.window,
            element_window: state.element_window,
        }
    }
    fn window_order(&self, _: pid_t) -> Vec<u32> {
        self.state.borrow_mut().order_reads += 1;
        vec![2, 1]
    }
    fn post_records(
        &self,
        _: pid_t,
        current: Option<u32>,
        target: u32,
        style: RecordStyle,
    ) -> Option<RecordPosts> {
        self.focus_epoch.set(self.focus_epoch.get() + 1);
        let mut state = self.state.borrow_mut();
        state.posts.push((current, target, style));
        let obeys = match style {
            RecordStyle::Fast => self.obeys_fast,
            RecordStyle::Full => self.obeys_records,
        };
        if obeys {
            state.window = Some(target);
            state.element_window = Some(target);
        }
        Some(RecordPosts {
            posted: 4,
            failed: 0,
        })
    }
    fn await_focus(&self, _: pid_t, _: u32, duration: std::time::Duration) {
        self.state.borrow_mut().waits.push(duration);
    }
    fn needs_full_records(&self, pid: pid_t) -> bool {
        self.needs_full.borrow().contains(&pid)
    }
    fn note_needs_full_records(&self, pid: pid_t) {
        self.needs_full.borrow_mut().push(pid);
    }
    fn set_focused_window(&self, _: pid_t, wid: u32) -> bool {
        self.focus_epoch.set(self.focus_epoch.get() + 1);
        let mut state = self.state.borrow_mut();
        state.ax_writes.push(wid);
        if self.obeys_ax {
            state.window = Some(wid);
        }
        true
    }
    fn belief_match(&self, pid: pid_t, wid: u32) -> BeliefMatch {
        self.sync_notifications(pid);
        let result = self.beliefs.lookup(
            pid,
            self.incarnation.get(),
            wid,
            self.now.get(),
            self.focus_epoch.get(),
        );
        if result == BeliefMatch::FreshKeyAndActive && !self.app_active.get() {
            BeliefMatch::FreshKeyInactive
        } else {
            result
        }
    }
    fn belief_ticket(&self, pid: pid_t) -> Option<Ticket> {
        self.sync_notifications(pid);
        let stamp = self.incarnation.get()?;
        let _ = self.beliefs.begin(pid, stamp, self.focus_epoch.get());
        self.beliefs
            .set_observing(pid, stamp, self.observer_registered.get());
        self.observer_registered
            .get()
            .then(|| self.beliefs.begin(pid, stamp, self.focus_epoch.get()))
    }
    fn belief_now(&self) -> std::time::Instant {
        self.now.get()
    }
    fn publish_belief(
        &self,
        ticket: Ticket,
        wid: u32,
        focus: KeyFocus,
        confirmed_at: std::time::Instant,
    ) {
        self.sync_notifications(ticket.pid);
        if self.standard_window.get()
            && self.incarnation.get() == Some(ticket.stamp)
            && focus.window == Some(wid)
            && focus.element_window == Some(wid)
        {
            let _ = self.beliefs.publish(
                ticket,
                wid,
                self.app_active.get(),
                confirmed_at,
                self.focus_epoch.get(),
            );
        }
    }
    fn activate_believed_window(&self, _: pid_t, _: u32) -> bool {
        self.activation_posts.set(self.activation_posts.get() + 1);
        self.app_active.set(true);
        self.focus_epoch.set(self.focus_epoch.get() + 1);
        true
    }
}

impl FakeEnv {
    fn sync_notifications(&self, pid: pid_t) {
        let current = self.notification_generation.get();
        if self.seen_notification.replace(current) != current {
            if let Some(stamp) = self.incarnation.get() {
                self.beliefs.invalidate(pid, stamp);
            }
        }
    }
}

fn ops() -> ScopeOps {
    ScopeOps {
        window_order: |_| vec![2, 1],
        hand_back: |_, _, _, _| HandBack::Restored,
    }
}

fn enter(env: &FakeEnv) -> Result<WindowKeyScope, ScopeEntryError> {
    enter_scope_with(env, ops(), -78_001, 2, FastPathEligibility::Ineligible)
}

// The baseline has one entry path; the eligible path must be selected
// explicitly once window-only key entry is added.
fn enter_eligible(env: &FakeEnv) -> Result<WindowKeyScope, ScopeEntryError> {
    enter_scope_with(env, ops(), -78_001, 2, FastPathEligibility::Eligible)
}

fn enter_other_eligible(env: &FakeEnv, target: u32) -> Result<WindowKeyScope, ScopeEntryError> {
    enter_scope_with(env, ops(), -78_001, target, FastPathEligibility::Eligible)
}

#[test]
fn a_confirmed_window_reuses_belief_without_entry_focus_reads() {
    let _serial = key_scope_tests_serial();
    let env = FakeEnv::new(2);
    let first = enter_eligible(&env).expect("first scope confirms key focus");
    assert_eq!(first.finish(), HandBack::NotMoved);
    env.state.borrow_mut().key_focus_reads = 0;
    let second = enter_eligible(&env).expect("second scope reuses the confirmation");
    assert_eq!(second.primed(), KeyPrime::AlreadyKey);
    assert_eq!(env.state.borrow().key_focus_reads, 0);
    assert!(env.state.borrow().posts.is_empty());
    assert_eq!(second.finish(), HandBack::NotMoved);
}

#[test]
fn unreadable_or_sibling_element_does_not_publish_window_belief() {
    let _serial = key_scope_tests_serial();
    let mut all_warmed = true;
    for evidence in [None, Some(1), Some(888)] {
        let mut env = FakeEnv::new(2);
        let first = enter_eligible(&env).expect("establish confirmed target");
        assert_eq!(first.finish(), HandBack::NotMoved);
        env.state.borrow_mut().key_focus_reads = 0;
        let warm = enter_eligible(&env).expect("reuse confirmed target");
        all_warmed &= env.state.borrow().key_focus_reads == 0;
        assert_eq!(warm.finish(), HandBack::NotMoved);

        env.focus_epoch.set(env.focus_epoch.get() + 1);
        env.obeys_fast = false;
        env.state.borrow_mut().element_window = evidence;
        let mut unproven = enter_eligible(&env).expect("window can route without publishing");
        unproven.done = true;
        env.state.borrow_mut().key_focus_reads = 0;
        let mut again = enter_eligible(&env).expect("unproven focus is read again");
        assert!(
            env.state.borrow().key_focus_reads > 0,
            "{evidence:?} published belief"
        );
        again.done = true;
    }
    assert!(all_warmed, "cache must be warm before testing invalidation");
}

fn warm_eligible_belief(env: &FakeEnv) -> bool {
    assert!(env.observer_registered.get());
    assert_eq!(enter_eligible(env).unwrap().finish(), HandBack::NotMoved);
    env.state.borrow_mut().key_focus_reads = 0;
    assert_eq!(enter_eligible(env).unwrap().finish(), HandBack::NotMoved);
    env.state.borrow().key_focus_reads == 0
}

#[test]
fn reused_pid_does_not_inherit_the_old_incarnations_belief() {
    let _serial = key_scope_tests_serial();
    let env = FakeEnv::new(2);
    let warmed = warm_eligible_belief(&env);
    env.incarnation.set(Some((1, 2)));
    env.state.borrow_mut().key_focus_reads = 0;
    assert_eq!(enter_eligible(&env).unwrap().finish(), HandBack::NotMoved);
    assert!(env.state.borrow().key_focus_reads > 0);
    assert!(warmed, "cache must be warm before pid reuse");
}

#[test]
fn missing_process_forgets_its_belief_before_the_pid_returns() {
    let _serial = key_scope_tests_serial();
    let env = FakeEnv::new(2);
    let warmed = warm_eligible_belief(&env);
    let stamp = env.incarnation.get();
    env.incarnation.set(None);
    assert_eq!(enter_eligible(&env).unwrap().finish(), HandBack::NotMoved);
    env.incarnation.set(stamp);
    env.state.borrow_mut().key_focus_reads = 0;
    assert_eq!(enter_eligible(&env).unwrap().finish(), HandBack::NotMoved);
    assert!(env.state.borrow().key_focus_reads > 0);
    assert!(warmed, "cache must be warm before process exit");
}

#[test]
fn belief_is_fresh_before_250_ms_and_stale_at_250_ms() {
    let _serial = key_scope_tests_serial();
    let env = FakeEnv::new(2);
    let warmed = warm_eligible_belief(&env);
    let start = env.now.get();
    env.now.set(start + std::time::Duration::from_millis(249));
    env.state.borrow_mut().key_focus_reads = 0;
    assert_eq!(enter_eligible(&env).unwrap().finish(), HandBack::NotMoved);
    let reads_at_249 = env.state.borrow().key_focus_reads;
    env.now.set(start + std::time::Duration::from_millis(250));
    env.state.borrow_mut().key_focus_reads = 0;
    assert_eq!(enter_eligible(&env).unwrap().finish(), HandBack::NotMoved);
    assert!(env.state.borrow().key_focus_reads > 0);
    assert_eq!((warmed, reads_at_249), (true, 0));
}

#[test]
fn belief_for_one_window_never_matches_another_window() {
    let _serial = key_scope_tests_serial();
    let env = FakeEnv::new(2);
    let warmed = warm_eligible_belief(&env);
    env.state.borrow_mut().key_focus_reads = 0;
    let mut scope = enter_other_eligible(&env, 1).expect("other window is primed");
    assert!(env.state.borrow().key_focus_reads > 0);
    assert_eq!(scope.primed(), KeyPrime::KeyRecords);
    scope.done = true;
    assert!(warmed);
}

#[test]
fn ineligible_callers_still_read_focus_even_when_belief_is_warm() {
    let _serial = key_scope_tests_serial();
    let env = FakeEnv::new(2);
    let warmed = warm_eligible_belief(&env);
    env.state.borrow_mut().key_focus_reads = 0;
    assert_eq!(enter(&env).unwrap().finish(), HandBack::NotMoved);
    assert!(env.state.borrow().key_focus_reads > 0);
    assert!(warmed);
}

#[test]
fn focus_notification_invalidates_a_warm_belief() {
    let _serial = key_scope_tests_serial();
    let env = FakeEnv::new(2);
    let warmed = warm_eligible_belief(&env);
    env.notification_generation
        .set(env.notification_generation.get() + 1);
    env.state.borrow_mut().key_focus_reads = 0;
    assert_eq!(enter_eligible(&env).unwrap().finish(), HandBack::NotMoved);
    assert!(env.state.borrow().key_focus_reads > 0);
    assert!(
        env.state.borrow().posts.is_empty(),
        "settled stale focus needs no records"
    );
    assert!(warmed);
}

fn final_mismatch_does_not_post(moved_to: u32) {
    use crate::ax::exact_target::WindowFocusConflict;
    let _serial = key_scope_tests_serial();
    let env = FakeEnv::new(2);
    let warmed = warm_eligible_belief(&env);
    let scope = enter_eligible(&env).expect("enter before focus changes");
    // The notification is deliberately undelivered when the key window moves.
    env.state.borrow_mut().window = Some(moved_to);
    let key_posts = std::cell::Cell::new(0);
    let checked = crate::tools::press_key::verify_untargeted_key_with(
        -78_001,
        2,
        true,
        || env.state.borrow().window,
        |_| WindowFocusConflict::Clear,
    )
    .map(|()| key_posts.set(key_posts.get() + 1));
    assert!(checked.is_err());
    assert_eq!(key_posts.get(), 0, "no key goes to the changed focus");
    assert_eq!(scope.finish(), HandBack::NotMoved);
    assert!(
        warmed,
        "cache must be warm before the undelivered notification"
    );
}

#[test]
fn delayed_notification_with_a_final_window_mismatch_sends_no_key() {
    final_mismatch_does_not_post(888);
}

#[test]
fn sheet_as_final_focused_window_never_receives_the_parent_key() {
    final_mismatch_does_not_post(999);
}

#[test]
fn warm_belief_with_sibling_focused_element_refuses_before_post() {
    let _serial = key_scope_tests_serial();
    let env = FakeEnv::new(2);
    let warmed = warm_eligible_belief(&env);
    let scope = enter_eligible(&env).expect("enter before the element moves");
    env.state.borrow_mut().element_window = Some(888);
    let key_posts = std::cell::Cell::new(0);
    let checked = crate::tools::press_key::verify_untargeted_key_with(
        -78_001,
        2,
        true,
        || env.state.borrow().window,
        |sample| {
            crate::ax::exact_target::decide_window_focus_conflict(
                2,
                None,
                false,
                env.state.borrow().element_window,
                sample.flatten(),
                false,
            )
        },
    )
    .map(|()| key_posts.set(key_posts.get() + 1));
    assert!(checked.is_err(), "sibling conflict must remain a refusal");
    assert_eq!(key_posts.get(), 0);
    assert_eq!(scope.finish(), HandBack::NotMoved);
    assert!(
        warmed,
        "cache must be warm before the undelivered notification"
    );
}

#[test]
fn stale_mismatching_belief_runs_fast_records_to_target() {
    let _serial = key_scope_tests_serial();
    let env = FakeEnv::new(2);
    let warmed = warm_eligible_belief(&env);
    env.notification_generation
        .set(env.notification_generation.get() + 1);
    {
        let mut state = env.state.borrow_mut();
        state.window = Some(1);
        state.element_window = Some(1);
        state.key_focus_reads = 0;
    }
    let mut scope = enter_eligible(&env).expect("fast records confirm");
    assert_eq!(scope.primed(), KeyPrime::KeyRecords);
    assert!(env.state.borrow().key_focus_reads > 0);
    assert_eq!(
        env.state.borrow().posts,
        vec![(Some(1), 2, RecordStyle::Fast)]
    );
    scope.done = true;
    assert!(warmed);
}

#[test]
fn inactive_fresh_belief_posts_activation_only() {
    let _serial = key_scope_tests_serial();
    let env = FakeEnv::new(2);
    let warmed = warm_eligible_belief(&env);
    env.app_active.set(false);
    env.state.borrow_mut().key_focus_reads = 0;
    let scope = enter_eligible(&env).expect("activation-only path succeeds");
    assert_eq!(scope.primed(), KeyPrime::AlreadyKey);
    let reads = env.state.borrow().key_focus_reads;
    let activations = env.activation_posts.get();
    assert!(env.state.borrow().posts.is_empty());
    assert_eq!(scope.finish(), HandBack::NotMoved);
    assert_eq!((warmed, reads, activations), (true, 0, 1));
}

#[test]
fn notification_during_confirmation_prevents_belief_publication() {
    let _serial = key_scope_tests_serial();
    let env = FakeEnv::new(2);
    let warmed = warm_eligible_belief(&env);
    env.notification_generation
        .set(env.notification_generation.get() + 1);
    env.notify_during_focus_read.set(2);
    assert_eq!(enter_eligible(&env).unwrap().finish(), HandBack::NotMoved);
    env.state.borrow_mut().key_focus_reads = 0;
    assert_eq!(enter_eligible(&env).unwrap().finish(), HandBack::NotMoved);
    assert!(
        env.state.borrow().key_focus_reads > 0,
        "a stale ticket published"
    );
    assert!(warmed);
}

#[test]
fn driver_mutation_during_confirmation_prevents_belief_publication() {
    let _serial = key_scope_tests_serial();
    let env = FakeEnv::new(2);
    let warmed = warm_eligible_belief(&env);
    env.focus_epoch.set(env.focus_epoch.get() + 1);
    env.mutation_during_focus_read.set(2);
    assert_eq!(enter_eligible(&env).unwrap().finish(), HandBack::NotMoved);
    env.state.borrow_mut().key_focus_reads = 0;
    assert_eq!(enter_eligible(&env).unwrap().finish(), HandBack::NotMoved);
    assert!(
        env.state.borrow().key_focus_reads > 0,
        "a stale mutation ticket published"
    );
    assert!(warmed);
}

#[test]
fn failed_observer_registration_never_produces_a_fresh_belief() {
    let _serial = key_scope_tests_serial();
    let env = FakeEnv::new(2);
    let warmed = warm_eligible_belief(&env);
    env.incarnation.set(Some((1, 2)));
    env.observer_registered.set(false);
    assert!(!env.observer_registered.get());
    assert_eq!(enter_eligible(&env).unwrap().finish(), HandBack::NotMoved);
    env.state.borrow_mut().key_focus_reads = 0;
    assert_eq!(enter_eligible(&env).unwrap().finish(), HandBack::NotMoved);
    assert!(env.state.borrow().key_focus_reads > 0);
    assert!(warmed);
}

#[test]
fn sheet_target_never_reuses_a_standard_window_belief() {
    let _serial = key_scope_tests_serial();
    let env = FakeEnv::new(2);
    let warmed = warm_eligible_belief(&env);
    env.notification_generation
        .set(env.notification_generation.get() + 1);
    env.standard_window.set(false);
    assert!(!env.standard_window.get());
    assert_eq!(enter_eligible(&env).unwrap().finish(), HandBack::NotMoved);
    env.state.borrow_mut().key_focus_reads = 0;
    assert_eq!(enter_eligible(&env).unwrap().finish(), HandBack::NotMoved);
    assert!(
        env.state.borrow().key_focus_reads > 0,
        "sheet was fast-pathed"
    );
    assert!(warmed);
}

#[test]
fn fast_confirmation_budget_precedes_full_fallback_and_remembers_it() {
    let _serial = key_scope_tests_serial();
    let mut env = FakeEnv::new(1);
    env.obeys_fast = false;
    let mut scope = enter(&env).expect("full records confirm the target");
    assert_eq!(
        env.state.borrow().waits,
        vec![
            std::time::Duration::from_millis(40),
            std::time::Duration::from_millis(520)
        ]
    );
    assert!(env.needs_full_records(-78_001));
    scope.done = true;
}

#[test]
fn a_frontmost_app_whose_other_window_is_key_gets_the_target_window_key() {
    let _serial = key_scope_tests_serial();
    // The entry reads no front state: it enters an app that is
    // frontmost (or whose front state is unreadable) like any other.
    let env = FakeEnv::new(1);
    let mut scope = enter(&env).unwrap_or_else(|e| panic!("{}", e.reason));
    assert_eq!(scope.primed(), KeyPrime::KeyRecords);
    assert_eq!(
        env.state.borrow().posts,
        vec![(Some(1), 2, RecordStyle::Fast)],
        "the short form alone makes the window key"
    );
    assert_eq!(scope.entry().records_posted, 4);
    scope.done = true;
}

/// An app that ignores the short form still gets the full sequence, and
/// the scope enters only once that is confirmed.
#[test]
fn an_app_that_ignores_the_fast_form_gets_the_full_sequence() {
    let _serial = key_scope_tests_serial();
    let mut env = FakeEnv::new(1);
    env.obeys_fast = false;
    let mut scope = enter(&env).expect("the full sequence confirms");
    assert_eq!(scope.primed(), KeyPrime::KeyRecords);
    assert_eq!(
        env.state.borrow().posts,
        vec![
            (Some(1), 2, RecordStyle::Fast),
            (Some(1), 2, RecordStyle::Full)
        ]
    );
    assert_eq!(scope.entry().records_posted, 8);
    scope.done = true;
}

/// An app that needed the full sequence once is not asked for the short
/// form again: its next entry and its hand-back go straight to the full
/// one, so a slow app pays the short attempt only once.
#[test]
fn an_app_that_needed_the_full_sequence_skips_the_short_form_next_time() {
    let _serial = key_scope_tests_serial();
    let mut env = FakeEnv::new(1);
    env.obeys_fast = false;
    let mut first = enter(&env).expect("the full sequence confirms");
    first.done = true;
    env.state.borrow_mut().window = Some(1);
    env.state.borrow_mut().element_window = Some(1);
    env.state.borrow_mut().posts.clear();
    let mut second = enter(&env).expect("the full sequence confirms");
    assert_eq!(
        env.state.borrow().posts,
        vec![(Some(1), 2, RecordStyle::Full)],
        "no short attempt for an app that did not take it"
    );
    second.done = true;
    // The hand-back skips it too.
    env.state.borrow_mut().posts.clear();
    let outcome = run_hand_back_with(&env, -78_001, 2, KeyPrime::KeyRecords, 1);
    assert_eq!(outcome, HandBack::Restored);
    assert_eq!(
        env.state.borrow().posts,
        vec![(Some(2), 1, RecordStyle::Full)]
    );
}

/// An app that took the short form is never remembered as slow.
#[test]
fn an_app_that_took_the_short_form_is_not_marked_slow() {
    let _serial = key_scope_tests_serial();
    let env = FakeEnv::new(1);
    let mut scope = enter(&env).expect("the short form confirms");
    scope.done = true;
    assert!(!env.needs_full_records(-78_001));
}

#[test]
fn a_target_window_that_is_already_key_posts_nothing_and_reads_no_window_order() {
    let _serial = key_scope_tests_serial();
    let env = FakeEnv::new(2);
    let scope = enter(&env).expect("a key window enters");
    assert_eq!(scope.primed(), KeyPrime::AlreadyKey);
    assert!(env.state.borrow().posts.is_empty());
    assert!(env.state.borrow().ax_writes.is_empty());
    assert_eq!(env.state.borrow().order_reads, 0);
    assert_eq!(scope.finish(), HandBack::NotMoved);
}

#[test]
fn a_hand_back_is_posted_whichever_app_is_frontmost() {
    let env = FakeEnv::new(2);
    let outcome = run_hand_back_with(&env, -78_002, 2, KeyPrime::KeyRecords, 1);
    assert_eq!(outcome, HandBack::Restored);
    assert_eq!(
        env.state.borrow().posts,
        vec![(Some(2), 1, RecordStyle::Fast)]
    );
    // An app that ignores the short form is handed key back with the
    // full sequence.
    let mut slow = FakeEnv::new(2);
    slow.obeys_fast = false;
    let outcome = run_hand_back_with(&slow, -78_002, 2, KeyPrime::KeyRecords, 1);
    assert_eq!(outcome, HandBack::Restored);
    assert_eq!(
        slow.state.borrow().posts,
        vec![
            (Some(2), 1, RecordStyle::Fast),
            (Some(2), 1, RecordStyle::Full)
        ]
    );
    let env = FakeEnv::new(2);
    let outcome = run_hand_back_with(&env, -78_002, 2, KeyPrime::AxFocusedWindow, 1);
    assert_eq!(outcome, HandBack::Restored);
    assert_eq!(env.state.borrow().ax_writes, vec![1]);
}

/// The rest of entering is unchanged: a window that cannot be confirmed
/// as key refuses with no key sent, and hands key back what it moved.
#[test]
fn an_unconfirmed_key_window_still_refuses() {
    let _serial = key_scope_tests_serial();
    let mut env = FakeEnv::new(1);
    env.obeys_fast = false;
    env.obeys_records = false;
    env.obeys_ax = false;
    let error = enter(&env).err().expect("unconfirmed key refuses");
    assert!(
        error
            .reason
            .contains("could not be confirmed as the app's key window"),
        "{}",
        error.reason
    );
    assert!(
        error.reason.contains("no keys were sent"),
        "{}",
        error.reason
    );
    assert!(error.entry.records_posted > 0 && error.entry.ax_write);
    assert!(error.hand_back.is_some(), "what was moved is handed back");
}
