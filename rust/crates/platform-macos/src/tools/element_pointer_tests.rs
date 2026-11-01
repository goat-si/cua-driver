use super::*;
use crate::ax::bindings::{kAXErrorIllegalArgument, kAXErrorSuccess};
use crate::input::mouse::post_sink::{capture_events, CapturedEvent};
use cua_driver_core::tool::Tool;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Mutex};

const WINDOW: u32 = 7;
const RECT: [f64; 4] = [100.0, 100.0, 50.0, 20.0];
const BOUNDS: [f64; 4] = [0.0, 0.0, 800.0, 600.0];
const CENTER: (f64, f64) = (125.0, 110.0);

/// One post: where, how many clicks, foreground, and the events handed over.
type Post = (ElementPoint, usize, bool, Vec<CapturedEvent>);

fn ancestors(chain: &[(&str, bool)]) -> Vec<(Option<String>, bool)> {
    chain
        .iter()
        .map(|(role, is_hit)| (Some((*role).to_owned()), *is_hit))
        .collect()
}

#[test]
fn a_hit_on_the_targets_own_cell_counts_as_the_target() {
    // KakaoTalk's chat list: the row title is an AXStaticText inside an AXCell
    // that answers the hit test for the whole row (goat-si/goat#2313).
    let chain = ancestors(&[("AXGroup", false), ("AXCell", true), ("AXRow", false)]);
    assert_eq!(
        hit_outcome(true, false, Some("AXCell".into()), chain),
        Hit::Target
    );
}

#[test]
fn a_cell_that_is_not_the_targets_nearest_cell_stays_elsewhere() {
    let elsewhere = || Hit::Elsewhere("AXCell".into());
    // Another row's cell.
    let other_row = ancestors(&[("AXCell", false), ("AXRow", false), ("AXTable", false)]);
    assert_eq!(
        hit_outcome(true, false, Some("AXCell".into()), other_row),
        elsewhere()
    );
    // An outer cell past the target's own cell.
    let outer = ancestors(&[("AXCell", false), ("AXTable", false), ("AXCell", true)]);
    assert_eq!(
        hit_outcome(true, false, Some("AXCell".into()), outer),
        elsewhere()
    );
    // The target's cell, but in another window.
    let own = ancestors(&[("AXCell", true)]);
    assert_eq!(
        hit_outcome(false, false, Some("AXCell".into()), own),
        elsewhere()
    );
    // A non-cell ancestor that answers the hit test (a whole list or window).
    let list = ancestors(&[("AXCell", false), ("AXTable", true)]);
    assert_eq!(
        hit_outcome(true, false, Some("AXTable".into()), list),
        Hit::Elsewhere("AXTable".into())
    );
    // A walk that passes the window boundary stops there.
    let past_window = ancestors(&[("AXWindow", false), ("AXCell", true)]);
    assert_eq!(
        hit_outcome(true, false, Some("AXCell".into()), past_window),
        elsewhere()
    );
    // An ancestor whose role cannot be read may be the nearest cell.
    let unreadable = vec![(None, false), (Some("AXCell".to_owned()), true)];
    assert_eq!(
        hit_outcome(true, false, Some("AXCell".into()), unreadable),
        elsewhere()
    );
}
/// A refusal code and the fake change that should produce it.
type Case = (&'static str, Box<dyn Fn(&mut Fake)>);

/// A target with a window at the origin and the element fully inside it.
struct Fake {
    pid: i32,
    permitted: bool,
    rect: Option<[f64; 4]>,
    /// Rect read on the second and later reads (an element that moved).
    rect_later: Option<Option<[f64; 4]>>,
    bounds: Option<[f64; 4]>,
    hit: Hit,
    /// Hit returned from the second hit test on (what the pointer move or
    /// primer changed under the point).
    hit_later: Option<Hit>,
    /// The hit-test read from which hit_later answers.
    hit_change_at: usize,
    /// Whether the background prologue reports that it activated the window.
    prologue_activates: bool,
    hit_reads: AtomicUsize,
    lists_ax_open: bool,
    ax_open_result: AXError,
    activates: bool,
    prologue_refuses: bool,
    rect_reads: AtomicUsize,
    calls: Mutex<Vec<&'static str>>,
    posts: Mutex<Vec<Post>>,
    /// Signals that post started, then waits for a release.
    block: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
}

impl Fake {
    fn new(pid: i32) -> Self {
        Self {
            pid,
            permitted: true,
            rect: Some(RECT),
            rect_later: None,
            bounds: Some(BOUNDS),
            hit: Hit::Target,
            hit_later: None,
            hit_change_at: 1,
            prologue_activates: true,
            hit_reads: AtomicUsize::new(0),
            lists_ax_open: false,
            ax_open_result: kAXErrorSuccess,
            activates: true,
            prologue_refuses: false,
            rect_reads: AtomicUsize::new(0),
            calls: Mutex::new(Vec::new()),
            posts: Mutex::new(Vec::new()),
            block: Mutex::new(None),
        }
    }

    fn note(&self, call: &'static str) {
        self.calls.lock().unwrap().push(call);
    }

    fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }

    fn posts(&self) -> Vec<Post> {
        self.posts.lock().unwrap().clone()
    }
}

#[async_trait]
impl ElementTarget for Fake {
    async fn gate(&self, _action: BackgroundAction) -> Result<BackgroundMutationLease, ToolResult> {
        self.note("gate");
        Ok(crate::tools::acquire_background_mutation(self.pid).await)
    }
    fn web_select_refusal(&self) -> Option<ToolResult> {
        None
    }
    fn lists_ax_open(&self) -> bool {
        self.lists_ax_open
    }
    fn ax_open(&self) -> AXError {
        self.note("ax_open");
        self.ax_open_result
    }
    fn pointer_permitted(&self, _foreground: bool) -> bool {
        self.note("pointer_permitted");
        self.permitted
    }
    fn element_rect(&self) -> Option<[f64; 4]> {
        self.note("rect");
        let read = self.rect_reads.fetch_add(1, Ordering::SeqCst);
        match (read, self.rect_later) {
            (0, _) | (_, None) => self.rect,
            (_, Some(later)) => later,
        }
    }
    fn window_bounds(&self) -> Option<[f64; 4]> {
        self.bounds
    }
    fn hit_at(&self, point: (f64, f64)) -> Hit {
        assert_eq!(point, CENTER);
        let read = self.hit_reads.fetch_add(1, Ordering::SeqCst);
        match &self.hit_later {
            Some(later) if read >= self.hit_change_at => later.clone(),
            _ => self.hit.clone(),
        }
    }
    fn post(
        &self,
        point: &ElementPoint,
        count: usize,
        foreground: bool,
        before_target: &mut dyn FnMut() -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        self.note("post");
        if let Some((entered, release)) = self.block.lock().unwrap().take() {
            entered.send(()).unwrap();
            release.recv().unwrap();
        }
        // The real primitive, with the transport captured.
        let (result, events) = capture_events(true, || {
            crate::input::mouse::click_at_xy_with_window_local_checked(
                self.pid,
                point.screen,
                point.window_local,
                WINDOW,
                count,
                &[],
                crate::input::mouse::WindowClickDelivery::from_foreground(foreground),
                before_target,
            )
        });
        self.posts
            .lock()
            .unwrap()
            .push((*point, count, foreground, events));
        result
    }
    fn in_foreground(&self, body: &mut dyn FnMut() -> anyhow::Result<()>) -> anyhow::Result<()> {
        self.note("activate");
        if self.activates {
            body()
        } else {
            anyhow::bail!("the window did not become focused")
        }
    }
    async fn background_pointer_prologue(
        &self,
        _prior_front: Option<i32>,
    ) -> Result<PointerPrologue, ToolResult> {
        self.note("prologue");
        if self.prologue_refuses {
            return Err(crate::tools::key_restore_pending_refusal(
                "click",
                Some(self.pid),
                Some(WINDOW),
            ));
        }
        Ok(PointerPrologue {
            activated: self.prologue_activates,
            prior_key_wid: None,
            key_focus: None,
        })
    }
    async fn restore_after_pointer(&self, _prior_front: Option<i32>, _prologue: PointerPrologue) {
        self.note("restore");
    }
}

fn position(calls: &[&str], call: &str) -> usize {
    calls
        .iter()
        .position(|seen| *seen == call)
        .unwrap_or_else(|| panic!("{call} not in {calls:?}"))
}

/// The down/up pairs a capture shows at the element's center, with their
/// click states (the background route's off-screen primer is elsewhere).
fn target_pairs(events: &[CapturedEvent]) -> Vec<(&str, i64)> {
    events
        .iter()
        .filter(|event| event.location == CENTER && event.event != "MouseMoved")
        .map(|event| (event.event.as_str(), event.click_state))
        .collect()
}

fn pairs(count: i64) -> Vec<(&'static str, i64)> {
    (1..=count)
        .flat_map(|state| [("LeftMouseDown", state), ("LeftMouseUp", state)])
        .collect()
}

/// The error under any activation tag.
fn inner(error: &anyhow::Error) -> &anyhow::Error {
    error
        .downcast_ref::<AfterActivation>()
        .map_or(error, |after| &after.source)
}

fn refusal_code(error: anyhow::Error) -> &'static str {
    inner(&error)
        .downcast_ref::<NotDispatched>()
        .expect("a refusal")
        .code
}

#[test]
fn only_an_integer_from_one_to_three_is_a_click_count() {
    assert_eq!(click_count(&json!({})).unwrap(), 1);
    for count in 1..=3 {
        assert_eq!(click_count(&json!({ "count": count })).unwrap(), count);
    }
    for bad in [
        json!(0),
        json!(-1),
        json!(4),
        json!(99),
        json!(2.5),
        json!("2"),
        Value::Null,
    ] {
        let refusal = click_count(&json!({ "count": bad })).unwrap_err();
        let structured = refusal.structured_content.expect("structured refusal");
        assert_eq!(structured["code"], "invalid_arguments", "{bad}");
        assert_eq!(structured["dispatched"], false, "{bad}");
    }
}

#[test]
fn an_element_double_click_hands_over_two_down_up_pairs_with_click_counts() {
    for foreground in [false, true] {
        let fake = Fake::new(1);
        let performed = run_blocking(&fake, 3, 2, false, foreground).unwrap();
        let posts = fake.posts();
        assert_eq!(posts.len(), 1);
        let (point, count, posted_foreground, events) = &posts[0];
        assert_eq!(point.screen, CENTER);
        assert_eq!(point.window_local, CENTER);
        assert_eq!((*count, *posted_foreground), (2, foreground));
        assert_eq!(target_pairs(events), pairs(2), "foreground {foreground}");
        let primers = events.iter().filter(|event| event.location == (-1.0, -1.0));
        assert_eq!(primers.count(), if foreground { 0 } else { 2 });
        assert_eq!(
            performed,
            Performed::Clicks(PointerClicks {
                point: *point,
                primer: !foreground
            })
        );
    }
}

#[test]
fn a_triple_click_hands_over_three_pairs_even_when_the_element_lists_axopen() {
    let mut fake = Fake::new(1);
    fake.lists_ax_open = true;
    run_blocking(&fake, 3, 3, false, false).unwrap();
    assert!(!fake.calls().contains(&"ax_open"));
    assert_eq!(target_pairs(&fake.posts()[0].3), pairs(3));
}

#[test]
fn a_double_click_on_an_element_listing_axopen_sends_axopen_and_no_clicks() {
    let fake = Fake::new(1);
    let performed = run_blocking(&fake, 3, 2, true, false).unwrap();
    assert!(matches!(performed, Performed::AxOpen(_)));
    assert_eq!(fake.calls(), ["ax_open"]);
    let result = performed_result(3, WINDOW, 2, false, &performed, "");
    let structured = result.structured_content.unwrap();
    assert_eq!(structured["performed"], "ax_open");
    assert_eq!(structured["clicks_posted"], 0);
    assert_eq!(structured["effect"], "unverifiable");
}

#[test]
fn an_axopen_that_may_have_opened_is_not_followed_by_clicks() {
    let mut fake = Fake::new(1);
    fake.ax_open_result = crate::ax::bindings::kAXErrorCannotComplete;
    for foreground in [false, true] {
        let performed = run_blocking(&fake, 3, 2, true, foreground).unwrap();
        assert!(matches!(&performed, Performed::AxOpen(m) if m.contains("may have opened")));
    }
    assert!(fake.posts().is_empty());
    // An AXOpen error proving nothing was sent falls back to checked clicks
    // in the foreground only.
    fake.ax_open_result = kAXErrorIllegalArgument;
    assert!(run_blocking(&fake, 3, 2, true, false).is_err());
    assert!(fake.posts().is_empty());
    run_blocking(&fake, 3, 2, true, true).unwrap();
    assert_eq!(target_pairs(&fake.posts()[0].3), pairs(2));
}

#[test]
fn a_point_that_is_not_proven_on_the_element_sends_nothing() {
    let cases: Vec<Case> = vec![
        ("pointer_not_permitted", Box::new(|f| f.permitted = false)),
        ("element_point_unresolved", Box::new(|f| f.rect = None)),
        (
            "element_point_unresolved",
            Box::new(|f| f.rect = Some([100.0, 100.0, 0.0, 20.0])),
        ),
        ("window_bounds_unavailable", Box::new(|f| f.bounds = None)),
        (
            "element_outside_window",
            Box::new(|f| f.bounds = Some([0.0, 0.0, 120.0, 600.0])),
        ),
        (
            "element_not_at_point",
            Box::new(|f| f.hit = Hit::Elsewhere("AXRow in another window".into())),
        ),
        ("element_not_at_point", Box::new(|f| f.hit = Hit::Nothing)),
        (
            "element_moved",
            Box::new(|f| f.rect_later = Some(Some([100.0, 140.0, 50.0, 20.0]))),
        ),
    ];
    for (code, setup) in cases {
        for foreground in [false, true] {
            let mut fake = Fake::new(1);
            setup(&mut fake);
            let error = run_blocking(&fake, 3, 2, false, foreground).unwrap_err();
            assert_eq!(refusal_code(error), code);
            assert!(fake.posts().is_empty(), "{code}");
        }
    }
}

#[test]
fn a_foreground_click_whose_window_cannot_be_fronted_checks_and_sends_nothing() {
    let mut fake = Fake::new(1);
    fake.activates = false;
    for use_ax_open in [false, true] {
        let error = run_blocking(&fake, 3, 2, use_ax_open, true).unwrap_err();
        // The activation helper may have fronted the window before it gave
        // up: the result says so and claims no clean refusal.
        let result = error_result(2, error);
        let structured = result.structured_content.clone().unwrap();
        assert_eq!(structured["code"], "foreground_unavailable");
        assert_eq!(structured["effect"], "unverifiable");
        assert_eq!(structured["activation"], "foreground_attempted");
        assert!(structured.get("dispatched").is_none(), "{structured}");
        let text = format!("{result:?}");
        assert!(text.contains("may already have been brought"), "{text}");
        assert!(!text.contains("nothing was sent"), "{text}");
    }
    assert_eq!(fake.calls().last(), Some(&"activate"));
    assert!(!fake.calls().contains(&"post"));
    assert!(!fake.calls().contains(&"ax_open"));
    assert!(fake.posts().is_empty());
}

#[test]
fn a_foreground_point_off_the_element_is_refused_before_activation() {
    let mut fake = Fake::new(1);
    fake.hit = Hit::Nothing;
    let error = run_blocking(&fake, 3, 2, false, true).unwrap_err();
    let result = error_result(2, error);
    let structured = result.structured_content.unwrap();
    assert_eq!(structured["code"], "element_not_at_point");
    assert_eq!(structured["dispatched"], false);
    assert!(!fake.calls().contains(&"activate"));
}

#[test]
fn a_foreground_point_that_fails_after_activation_is_unverifiable() {
    let mut fake = Fake::new(1);
    // The preflight passes; the check after activation does not.
    fake.hit_later = Some(Hit::Elsewhere("a sheet".into()));
    let error = run_blocking(&fake, 3, 2, false, true).unwrap_err();
    let result = error_result(2, error);
    let structured = result.structured_content.clone().unwrap();
    assert_eq!(structured["code"], "element_not_at_point");
    assert_eq!(structured["effect"], "unverifiable");
    assert_eq!(structured["activation"], "foreground_attempted");
    assert!(structured.get("dispatched").is_none(), "{structured}");
    assert!(fake.posts().is_empty());
    let text = format!("{result:?}");
    assert!(!text.contains("nothing was sent"), "{text}");
}

#[test]
fn a_target_that_changes_while_the_clicks_are_prepared_gets_no_click() {
    for (foreground, events_sent) in [(false, 3), (true, 1)] {
        let mut fake = Fake::new(1);
        fake.hit_later = Some(Hit::Elsewhere("a hover button".into()));
        // Changes at the recheck before the first click: the background has
        // one check before it, the foreground a preflight and one more.
        fake.hit_change_at = if foreground { 2 } else { 1 };
        let error = run_blocking(&fake, 3, 2, false, foreground).unwrap_err();
        let changed = inner(&error)
            .downcast_ref::<TargetChanged>()
            .expect("target changed");
        assert_eq!(
            changed,
            &TargetChanged {
                code: "element_not_at_point",
                events_sent,
                primer: !foreground,
            }
        );
        let posts = fake.posts();
        assert_eq!(posts.len(), 1);
        // The move (and background primer) went out; no target pair did.
        assert!(
            target_pairs(&posts[0].3).is_empty(),
            "foreground {foreground}"
        );
        assert_eq!(posts[0].3.len(), events_sent);
        let result = error_result(2, error);
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["code"], "element_changed_before_click");
        assert_eq!(structured["dispatched"], true);
        assert_eq!(structured["clicks_posted"], 0);
        assert_eq!(structured["effect"], "unverifiable");
    }
}

#[test]
fn a_coordinate_click_reports_target_clicks_and_the_primer_separately() {
    let point = ElementPoint {
        screen: CENTER,
        window_local: CENTER,
    };
    for (primer, primer_clicks) in [(true, 1), (false, 0)] {
        let performed = Performed::Clicks(PointerClicks { point, primer });
        let result = performed_result(3, WINDOW, 2, !primer, &performed, "");
        let record = result.action_record.clone().expect("action record");
        assert_eq!(record.delivered_count, Some(2));
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["performed"], "coordinate_clicks");
        assert_eq!(structured["count"], 2);
        assert_eq!(structured["clicks_posted"], 2);
        assert_eq!(structured["primer_clicks"], primer_clicks);
        assert_eq!(structured["effect"], "unverifiable");
        assert_eq!(structured["verified"], false);
    }
}

// ── Through the click tool ────────────────────────────────────────────────

fn tool_with_token(pid: i32) -> (super::super::click::ClickTool, String, Arc<ToolState>) {
    let state = Arc::new(ToolState::new(false, false, None));
    // A real AX element (of an app that does not exist), owned by the
    // snapshot; the registered fake answers for it.
    let element = unsafe { crate::ax::bindings::AXUIElementCreateApplication(pid) } as usize;
    let snapshot = state.snapshots.publish(
        pid,
        u64::from(WINDOW),
        crate::ax::snapshot::AxSnapshot {
            elements: vec![element],
        },
    );
    let token = cua_driver_core::element_token::token_for(snapshot, 0);
    (
        super::super::click::ClickTool::new(state.clone()),
        token,
        state,
    )
}

#[tokio::test]
#[ignore = "live: reads HID idle time; runs alone in the macOS live lane"]
async fn b4_review_simulate_refuses_a_different_hit_before_dispatch() {
    let pid = 1_000_151;
    let mut fake = Fake::new(pid);
    fake.hit = Hit::Elsewhere("overlay".into());
    let fake = Arc::new(fake);
    let _registered = test_targets::register(pid, fake.clone());
    let (tool, token, _) = tool_with_token(pid);
    let result = tool
        .invoke(json!({"pid":pid,"element_token":token,"simulate":true}))
        .await;
    let structured = result.structured_content.expect("refusal");
    assert_eq!(structured["code"], "element_not_at_point");
    assert_eq!(structured["dispatched"], false);
    assert!(fake.posts().is_empty());
}

#[tokio::test]
async fn b4_review_simulate_invalid_combinations_precede_token_resolution() {
    let tool = super::super::click::ClickTool::new(Arc::new(ToolState::new(false, false, None)));
    for extra in [
        json!({"count":2}),
        json!({"action":"open"}),
        json!({"button":"right"}),
    ] {
        let mut args = json!({"element_token":"s00000001:0", "simulate":true});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let result = tool.invoke(args).await;
        let structured = result.structured_content.expect("refusal");
        assert_eq!(structured["code"], "invalid_arguments");
        assert_eq!(structured["dispatched"], false);
    }
}

#[test]
fn b4_review_checked_matching_hit_is_accepted() {
    let fake = Fake::new(1_000_152);
    let args = json!({"pid":fake.pid, "element_token":"s00000001:0", "simulate":true});
    let pixel =
        super::super::element_simulate::checked_pixel_args(args, fake.pid, WINDOW, &fake).unwrap();
    assert_eq!(pixel["x"], CENTER.0);
    assert_eq!(pixel["y"], CENTER.1);
    assert_eq!(fake.hit_reads.load(Ordering::SeqCst), 1);
    assert!(fake.posts().is_empty());
}

#[tokio::test]
#[ignore = "live: reads HID idle time, NSWorkspace, SkyLight, window list; runs alone in the macOS live lane"]
async fn an_element_count_two_takes_the_checked_pointer_route() {
    let pid = 1_000_101;
    let fake = Arc::new(Fake::new(pid));
    let _registered = test_targets::register(pid, fake.clone());
    let (tool, token, _state) = tool_with_token(pid);
    let result = tool
        .invoke(json!({ "pid": pid, "element_token": token, "count": 2 }))
        .await;
    let structured = result
        .structured_content
        .clone()
        .expect("structured result");
    assert_eq!(structured["performed"], "coordinate_clicks", "{result:?}");
    assert_eq!(structured["count"], 2);
    assert_eq!(structured["clicks_posted"], 2);
    assert_eq!(structured["effect"], "unverifiable");
    let posts = fake.posts();
    assert_eq!(posts.len(), 1, "{result:?}");
    assert_eq!(posts[0].0.screen, CENTER);
    assert_eq!(target_pairs(&posts[0].3), pairs(2));
    // The background pixel-click prologue runs before the clicks and focus
    // is handed back after them.
    let calls = fake.calls();
    assert!(position(&calls, "gate") < position(&calls, "prologue"));
    assert!(position(&calls, "prologue") < position(&calls, "post"));
    assert!(position(&calls, "post") < position(&calls, "restore"));
}

#[tokio::test]
#[ignore = "live: reads HID idle time, NSWorkspace, SkyLight, event tap, window list; runs alone in the macOS live lane"]
async fn an_element_axopen_double_click_skips_the_pointer_prologue() {
    let pid = 1_000_106;
    let mut fake = Fake::new(pid);
    fake.lists_ax_open = true;
    let fake = Arc::new(fake);
    let _registered = test_targets::register(pid, fake.clone());
    let (tool, token, _state) = tool_with_token(pid);
    let result = tool
        .invoke(json!({ "pid": pid, "element_token": token, "count": 2 }))
        .await;
    let structured = result
        .structured_content
        .clone()
        .expect("structured result");
    assert_eq!(structured["performed"], "ax_open", "{result:?}");
    assert_eq!(fake.calls(), ["gate", "ax_open"]);
}

#[tokio::test]
#[ignore = "live: reads HID idle time, NSWorkspace, window list; runs alone in the macOS live lane"]
async fn a_point_off_the_element_is_refused_before_the_prologue() {
    let pid = 1_000_108;
    let mut fake = Fake::new(pid);
    fake.hit = Hit::Elsewhere("AXRow in another window".into());
    let fake = Arc::new(fake);
    let _registered = test_targets::register(pid, fake.clone());
    let (tool, token, _state) = tool_with_token(pid);
    let result = tool
        .invoke(json!({ "pid": pid, "element_token": token, "count": 2 }))
        .await;
    let structured = result
        .structured_content
        .clone()
        .expect("structured refusal");
    assert_eq!(structured["code"], "element_not_at_point", "{result:?}");
    assert_eq!(structured["dispatched"], false);
    assert!(!fake.calls().contains(&"prologue"));
    assert!(fake.posts().is_empty());
}

#[tokio::test]
#[ignore = "live: reads HID idle time, NSWorkspace, window list; runs alone in the macOS live lane"]
async fn a_point_that_fails_after_activation_is_unverifiable() {
    let pid = 1_000_109;
    let mut fake = Fake::new(pid);
    // The preflight passes; the check after the prologue does not.
    fake.hit_later = Some(Hit::Elsewhere("a sheet".into()));
    let fake = Arc::new(fake);
    let _registered = test_targets::register(pid, fake.clone());
    let (tool, token, _state) = tool_with_token(pid);
    let result = tool
        .invoke(json!({ "pid": pid, "element_token": token, "count": 2 }))
        .await;
    let structured = result
        .structured_content
        .clone()
        .expect("structured result");
    assert_eq!(structured["code"], "element_not_at_point", "{result:?}");
    assert_eq!(structured["effect"], "unverifiable");
    assert_eq!(structured["dispatched"], true);
    assert_eq!(structured["clicks_posted"], 0);
    assert!(fake.calls().contains(&"prologue"));
    assert!(fake.posts().is_empty());
    let text = format!("{result:?}");
    assert!(!text.contains("nothing was sent"), "{text}");
}

#[test]
fn a_recheck_failure_with_nothing_sent_is_a_refusal() {
    let changed = TargetChanged {
        code: "element_not_at_point",
        events_sent: 0,
        primer: false,
    };
    assert!(changed.to_string().contains("Nothing had been handed"));
    let result = error_result(2, changed.into());
    let structured = result.structured_content.unwrap();
    assert_eq!(structured["dispatched"], false);
    assert_eq!(structured["effect"], "refused");
    assert_eq!(structured["primer_clicks"], 0);
}

#[tokio::test]
#[ignore = "live: reads HID idle time, NSWorkspace, window list; runs alone in the macOS live lane"]
async fn an_unconfirmed_background_activation_is_reported_as_uncertain() {
    let pid = 1_000_110;
    let mut fake = Fake::new(pid);
    fake.prologue_activates = false;
    fake.hit_later = Some(Hit::Elsewhere("a sheet".into()));
    let fake = Arc::new(fake);
    let _registered = test_targets::register(pid, fake.clone());
    let (tool, token, _state) = tool_with_token(pid);
    let result = tool
        .invoke(json!({ "pid": pid, "element_token": token, "count": 2 }))
        .await;
    let structured = result
        .structured_content
        .clone()
        .expect("structured result");
    assert_eq!(structured["effect"], "unverifiable", "{result:?}");
    assert_eq!(structured["activation"], "background_uncertain");
    assert!(structured.get("dispatched").is_none(), "{structured}");
    let text = format!("{result:?}");
    assert!(text.contains("may have reached it"), "{text}");
    assert!(!text.contains("had already been made active"), "{text}");
}

#[test]
fn only_a_pending_key_hand_back_is_a_clean_prologue_refusal() {
    let code = |result: ToolResult| {
        let structured = result.structured_content.unwrap();
        (
            structured["code"].as_str().unwrap().to_owned(),
            structured["effect"].as_str().unwrap().to_owned(),
            structured.get("activation").cloned(),
        )
    };
    // No attempt: a clean refusal.
    let refused = prologue_outcome(1, WINDOW, Ok(None), || panic!("not read")).unwrap_err();
    assert_eq!(
        code(refused),
        ("key_restore_pending".into(), "refused".into(), None)
    );
    // An attempt that did not activate, with the sticky invalid-connection
    // flag set by an earlier call: unverifiable, never "nothing sent".
    let stale = prologue_outcome(1, WINDOW, Ok(Some(false)), || true).unwrap_err();
    assert_eq!(
        code(stale),
        (
            "windowserver_connection_invalid".into(),
            "unverifiable".into(),
            Some(json!("background_uncertain"))
        )
    );
    // The activation task failed after it may have attempted the record.
    let failed = prologue_outcome(1, WINDOW, Err("panicked".into()), || false).unwrap_err();
    assert_eq!(
        code(failed),
        (
            "background_unavailable".into(),
            "unverifiable".into(),
            Some(json!("background_uncertain"))
        )
    );
    // Otherwise the gesture goes on, certain or uncertain.
    assert!(prologue_outcome(1, WINDOW, Ok(Some(true)), || true).unwrap());
    assert!(!prologue_outcome(1, WINDOW, Ok(Some(false)), || false).unwrap());
}

#[tokio::test]
#[ignore = "live: reads HID idle time, NSWorkspace, window list; runs alone in the macOS live lane"]
async fn a_refused_pointer_prologue_sends_nothing() {
    let pid = 1_000_107;
    let mut fake = Fake::new(pid);
    fake.prologue_refuses = true;
    let fake = Arc::new(fake);
    let _registered = test_targets::register(pid, fake.clone());
    let (tool, token, _state) = tool_with_token(pid);
    let result = tool
        .invoke(json!({ "pid": pid, "element_token": token, "count": 2 }))
        .await;
    let structured = result
        .structured_content
        .clone()
        .expect("structured refusal");
    assert_eq!(structured["effect"], "refused", "{result:?}");
    // The point is proven first, then the prologue refuses.
    assert_eq!(
        fake.calls(),
        ["gate", "pointer_permitted", "rect", "rect", "prologue"]
    );
    assert!(fake.posts().is_empty());
}

#[tokio::test]
#[ignore = "live: reads HID idle time, SkyLight, accessibility IPC, session lock state, window list; runs alone in the macOS live lane"]
async fn an_element_count_one_keeps_the_existing_route() {
    let pid = 1_000_102;
    let fake = Arc::new(Fake::new(pid));
    let _registered = test_targets::register(pid, fake.clone());
    let (tool, token, _state) = tool_with_token(pid);
    let result = tool
        .invoke(json!({ "pid": pid, "element_token": token, "count": 1 }))
        .await;
    assert!(fake.calls().is_empty(), "{result:?}");
    assert_ne!(
        result.structured_content.unwrap_or_default()["performed"],
        "coordinate_clicks"
    );
}

#[tokio::test]
async fn a_bad_count_is_refused_before_anything_is_sent() {
    let pid = 1_000_103;
    let fake = Arc::new(Fake::new(pid));
    let _registered = test_targets::register(pid, fake.clone());
    let (tool, token, _state) = tool_with_token(pid);
    for bad in [
        json!(0),
        json!(-1),
        json!(4),
        json!(2.5),
        json!("2"),
        Value::Null,
    ] {
        for args in [
            json!({ "pid": pid, "element_token": token, "count": bad }),
            json!({ "pid": pid, "window_id": WINDOW, "x": 10, "y": 10, "count": bad }),
        ] {
            let result = tool.invoke(args).await;
            let structured = result
                .structured_content
                .clone()
                .expect("structured refusal");
            assert_eq!(structured["code"], "invalid_arguments", "{bad}: {result:?}");
            assert_eq!(structured["dispatched"], false, "{bad}");
        }
    }
    assert!(fake.calls().is_empty());
}

#[tokio::test]
#[ignore = "live: reads HID idle time; runs alone in the macOS live lane"]
async fn an_element_multi_click_needs_a_plain_left_press() {
    let pid = 1_000_104;
    let fake = Arc::new(Fake::new(pid));
    let _registered = test_targets::register(pid, fake.clone());
    let (tool, token, _state) = tool_with_token(pid);
    for extra in [
        json!({ "button": "right" }),
        json!({ "button": "middle" }),
        json!({ "action": "show_menu" }),
        json!({ "modifier": ["cmd"], "delivery_mode": "foreground", "window_id": WINDOW }),
    ] {
        let mut args = json!({ "pid": pid, "element_token": token, "count": 2 });
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let result = tool.invoke(args).await;
        let structured = result
            .structured_content
            .clone()
            .expect("structured refusal");
        assert_eq!(
            structured["code"], "invalid_arguments",
            "{extra}: {result:?}"
        );
    }
    assert!(fake.calls().is_empty());
}

#[test]
fn the_count_schema_matches_the_element_behavior() {
    let tool = super::super::click::ClickTool::new(Arc::new(ToolState::new(false, false, None)));
    let count = &tool.def().input_schema["properties"]["count"];
    assert_eq!(count["type"], "integer");
    let description = count["description"].as_str().unwrap();
    assert!(description.contains("1-3"), "{description}");
    assert!(!description.contains("pixel path only"), "{description}");
    assert!(description.contains("element_token"), "{description}");
    assert!(description.contains("AXOpen"), "{description}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: reads HID idle time, NSWorkspace, SkyLight, window list; runs alone in the macOS live lane"]
async fn cancelling_an_element_multi_click_keeps_the_lease_until_input_finishes() {
    let pid = 1_000_105;
    let fake = Arc::new(Fake::new(pid));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    *fake.block.lock().unwrap() = Some((entered_tx, release_rx));
    let _registered = test_targets::register(pid, fake.clone());
    let (tool, token, state) = tool_with_token(pid);
    let call = tokio::spawn(async move {
        tool.invoke(json!({
            "pid": pid,
            "element_token": token,
            "count": 2,
            "session": "ecc-cancelled-click",
        }))
        .await
    });
    // Bounded so a route that never reaches post fails instead of hanging.
    tokio::task::spawn_blocking(move || {
        entered_rx.recv_timeout(std::time::Duration::from_secs(30))
    })
    .await
    .unwrap()
    .expect("post started");
    call.abort();
    assert!(call.await.unwrap_err().is_cancelled());
    assert!(
        crate::background_mutation::try_acquire(pid).is_none(),
        "the lease outlives the cancelled caller while input runs"
    );
    // Focus-steal protection is armed for a background target whenever
    // another app is frontmost to restore to.
    if crate::apps::frontmost_pid().is_some_and(|front| front != pid) {
        assert!(
            crate::focus_steal::suppression_armed_for(pid),
            "focus protection outlives the cancelled caller while input runs"
        );
    }
    release_tx.send(()).unwrap();
    // Waits only until the blocking action returns and drops the lease.
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        crate::background_mutation::acquire(pid),
    )
    .await
    .expect("the lease is released once input finishes");
    assert_eq!(fake.posts().len(), 1);
    // The cursor followed the clicks the cancelled call still sent.
    let position = state
        .cursor_registry
        .get("ecc-cancelled-click")
        .and_then(|cursor| cursor.position)
        .expect("cursor moved");
    assert_eq!((position.x, position.y), CENTER);
}
