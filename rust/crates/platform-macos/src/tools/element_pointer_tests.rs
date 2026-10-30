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

/// A target with a window at the origin and the element fully inside it.
struct Fake {
    pid: i32,
    permitted: bool,
    rect: Option<[f64; 4]>,
    /// Rect read on the second and later reads (an element that moved).
    rect_later: Option<Option<[f64; 4]>>,
    bounds: Option<[f64; 4]>,
    hit: Hit,
    lists_ax_open: bool,
    ax_open_result: AXError,
    activates: bool,
    rect_reads: AtomicUsize,
    calls: Mutex<Vec<&'static str>>,
    posts: Mutex<Vec<(ElementPoint, usize, bool, Vec<CapturedEvent>)>>,
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
            lists_ax_open: false,
            ax_open_result: kAXErrorSuccess,
            activates: true,
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

    fn posts(&self) -> Vec<(ElementPoint, usize, bool, Vec<CapturedEvent>)> {
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
        self.hit.clone()
    }
    fn post(&self, point: &ElementPoint, count: usize, foreground: bool) -> anyhow::Result<()> {
        self.note("post");
        if let Some((entered, release)) = self.block.lock().unwrap().take() {
            entered.send(()).unwrap();
            release.recv().unwrap();
        }
        // The real primitive, with the transport captured.
        let (result, events) = capture_events(true, || {
            crate::input::mouse::click_at_xy_with_window_local(
                self.pid,
                point.screen.0,
                point.screen.1,
                point.window_local.0,
                point.window_local.1,
                WINDOW,
                count,
                &[],
                crate::input::mouse::WindowClickDelivery::from_foreground(foreground),
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

fn refusal_code(error: anyhow::Error) -> &'static str {
    error
        .downcast_ref::<NotDispatched>()
        .expect("a no-send refusal")
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
    let result = performed_result(3, WINDOW, 2, &performed, "");
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
    let cases: Vec<(&str, Box<dyn Fn(&mut Fake)>)> = vec![
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
        assert_eq!(refusal_code(error), "foreground_unavailable");
    }
    assert!(fake.calls().iter().all(|call| *call == "activate"));
    assert!(fake.posts().is_empty());
}

#[test]
fn a_coordinate_click_reports_target_clicks_and_the_primer_separately() {
    let point = ElementPoint {
        screen: CENTER,
        window_local: CENTER,
    };
    for (primer, primer_clicks) in [(true, 1), (false, 0)] {
        let performed = Performed::Clicks(PointerClicks { point, primer });
        let result = performed_result(3, WINDOW, 2, &performed, "");
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

fn tool_with_token(pid: i32) -> (super::super::click::ClickTool, String) {
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
    (super::super::click::ClickTool::new(state), token)
}

#[tokio::test]
async fn an_element_count_two_takes_the_checked_pointer_route() {
    let pid = 1_000_101;
    let fake = Arc::new(Fake::new(pid));
    let _registered = test_targets::register(pid, fake.clone());
    let (tool, token) = tool_with_token(pid);
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
}

#[tokio::test]
async fn an_element_count_one_keeps_the_existing_route() {
    let pid = 1_000_102;
    let fake = Arc::new(Fake::new(pid));
    let _registered = test_targets::register(pid, fake.clone());
    let (tool, token) = tool_with_token(pid);
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
    let (tool, token) = tool_with_token(pid);
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
async fn an_element_multi_click_needs_a_plain_left_press() {
    let pid = 1_000_104;
    let fake = Arc::new(Fake::new(pid));
    let _registered = test_targets::register(pid, fake.clone());
    let (tool, token) = tool_with_token(pid);
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
    assert_eq!(count["minimum"], 1);
    assert_eq!(count["maximum"], 3);
    let description = count["description"].as_str().unwrap();
    assert!(!description.contains("pixel path only"), "{description}");
    assert!(description.contains("element_token"), "{description}");
    assert!(description.contains("AXOpen"), "{description}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_an_element_multi_click_keeps_the_lease_until_input_finishes() {
    let pid = 1_000_105;
    let fake = Arc::new(Fake::new(pid));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    *fake.block.lock().unwrap() = Some((entered_tx, release_rx));
    let _registered = test_targets::register(pid, fake.clone());
    let (tool, token) = tool_with_token(pid);
    let call = tokio::spawn(async move {
        tool.invoke(json!({ "pid": pid, "element_token": token, "count": 2 }))
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
    release_tx.send(()).unwrap();
    // Waits only until the blocking action returns and drops the lease.
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        crate::background_mutation::acquire(pid),
    )
    .await
    .expect("the lease is released once input finishes");
    assert_eq!(fake.posts().len(), 1);
}
