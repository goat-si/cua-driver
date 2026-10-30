//! origin_scope, existing-profile scope requirement, and capability-manifest
//! narrowing against the deterministic mock CDP fixture.

use super::*;

const SCOPE_FIXTURE: &str = "https://fixture.test";

fn code(result: &ToolResult) -> String {
    result
        .structured_content
        .as_ref()
        .and_then(|value| value["refusal"]["code"].as_str())
        .unwrap_or_default()
        .to_owned()
}

fn is_refused(result: &ToolResult) -> bool {
    result
        .structured_content
        .as_ref()
        .is_some_and(|value| value["status"] == "refused")
}

fn page_input_calls(f: &Fixture) -> usize {
    f.state
        .lock()
        .unwrap()
        .calls
        .iter()
        .filter(|(_, method, _)| {
            method.starts_with("Input.")
                || method == "Runtime.callFunctionOn"
                || method == "Page.navigate"
        })
        .count()
}

async fn click(f: &Fixture, target: &str, tab: &str, button: &str, extra: Value) -> ToolResult {
    let mut args = json!({
        "target_id": target,
        "tab_id": tab,
        "ref": button,
        "input_route": "dom_event",
        "session": SESSION,
    });
    args.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    BrowserClickTool::new(f.engine.clone()).invoke(args).await
}

fn manifest_context(
    origins: &[&str],
    existing_profiles: &str,
) -> Arc<crate::session_authorization::EffectiveAuthorizationContext> {
    let source = format!(
        "version: 3\nresources:\n  browser:\n    origins: [{}]\n{existing_profiles}allow:\n  tools: [get_browser_state, browser_click, browser_navigate, browser_type]\n",
        origins
            .iter()
            .map(|origin| format!("\"{origin}\""))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("manifest.yaml");
    std::fs::write(&path, source).unwrap();
    let manifest = Arc::new(crate::session_manifest::load_manifest(&path).unwrap());
    crate::session_authorization::SessionAuthorizationRegistry::with_ceiling(
        crate::session_authorization::SessionModeCeiling::for_trusted_sessions(
            [crate::authorization::PermissionMode::Standard],
            false,
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(30),
        )
        .unwrap(),
    )
    .compatibility_context(
        crate::authorization::PermissionMode::Standard,
        Some(manifest),
    )
    .unwrap()
}

async fn read_in(
    f: &Fixture,
    target: &str,
    tab: &str,
    context: Arc<crate::session_authorization::EffectiveAuthorizationContext>,
    scoped: bool,
) -> ToolResult {
    let mut args = json!({ "target_id": target, "tab_id": tab, "session": SESSION });
    if scoped {
        args["origin_scope"] = json!([SCOPE_FIXTURE]);
    }
    let tool = GetBrowserStateTool::new(f.engine.clone());
    crate::tool::with_test_dispatch_context(context, tool.invoke(args)).await
}

// ── origin_scope on a driver-owned/embedded browser ──────────────────────────

#[tokio::test]
async fn a_write_outside_the_scope_is_refused_before_anything_is_sent() {
    let f = fixture().await;
    let (target, tab) = bind(&f).await;
    let snap = snapshot(&f, &target, &tab).await;
    let button = ref_of(&snap, "main", "main-btn");
    let before = page_input_calls(&f);

    let refused = click(
        &f,
        &target,
        &tab,
        &button,
        json!({"origin_scope": ["https://other.example"]}),
    )
    .await;
    assert_eq!(code(&refused), "browser_origin_outside_scope");
    assert_eq!(page_input_calls(&f), before, "nothing reaches the page");

    let allowed = click(
        &f,
        &target,
        &tab,
        &button,
        json!({"origin_scope": [SCOPE_FIXTURE]}),
    )
    .await;
    assert!(!is_refused(&allowed), "{:?}", allowed.structured_content);
    assert!(page_input_calls(&f) > before);
}

#[tokio::test]
async fn absent_scope_keeps_todays_behaviour_for_an_embedded_browser() {
    let f = fixture().await;
    let (target, tab) = bind(&f).await;
    let snap = snapshot(&f, &target, &tab).await;
    assert_eq!(snap["status"], "ok", "{snap}");
    let button = ref_of(&snap, "main", "main-btn");
    let clicked = click(&f, &target, &tab, &button, json!({})).await;
    assert!(!is_refused(&clicked), "{:?}", clicked.structured_content);
}

#[tokio::test]
async fn a_navigation_outside_the_scope_is_refused_and_not_sent() {
    let f = fixture().await;
    let (target, tab) = bind(&f).await;
    let refused = BrowserNavigateTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target, "tab_id": tab, "session": SESSION,
            "url": "https://other.example/",
            "origin_scope": [SCOPE_FIXTURE],
        }))
        .await;
    assert_eq!(code(&refused), "browser_origin_outside_scope");
    assert!(recorded_calls(&f, "Page.navigate").is_empty());
}

#[tokio::test]
async fn a_redirect_out_of_the_scope_is_refused_at_the_committed_document() {
    let f = fixture_with(|st| {
        st.navigate_commits = true;
        st.navigate_lands_on = Some("https://login.other.example/sso?next=secret".into());
    })
    .await;
    let (target, tab) = bind(&f).await;
    let refused = BrowserNavigateTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target, "tab_id": tab, "session": SESSION,
            "url": "https://fixture.test/account",
            "origin_scope": [SCOPE_FIXTURE],
        }))
        .await;
    let s = structured(&refused);
    assert_eq!(code(&refused), "browser_origin_outside_scope", "{s}");
    assert_eq!(s["refusal"]["detail"]["navigation_committed"], true);
    assert!(
        !s.to_string().contains("secret"),
        "no live address leaks: {s}"
    );
}

#[tokio::test]
async fn a_navigation_waits_for_its_own_commit_before_the_scope_check() {
    let f = fixture_with(|st| {
        st.navigate_commits = true;
        st.navigate_commit_after_reads = 2;
    })
    .await;
    let (target, tab) = bind(&f).await;
    let navigated = BrowserNavigateTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target, "tab_id": tab, "session": SESSION,
            "url": "https://fixture.test/next",
            "origin_scope": [SCOPE_FIXTURE],
        }))
        .await;
    assert_eq!(
        structured(&navigated)["status"],
        "ok",
        "{}",
        structured(&navigated)
    );
}

#[tokio::test]
async fn bind_with_a_scope_lists_out_of_scope_tabs_by_id_and_selection_only() {
    let f = fixture().await;
    let result = GetBrowserStateTool::new(f.engine.clone())
        .invoke(json!({
            "pid": 1, "window_id": 7, "session": SESSION,
            "origin_scope": ["https://other.example"],
        }))
        .await;
    let s = structured(&result);
    assert_eq!(s["status"], "ok", "{s}");
    let tab = s["tabs"][0].as_object().unwrap();
    let mut keys: Vec<_> = tab.keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["active", "tab_id"]);
    assert!(s["native_title"].is_null());
    assert!(!s.to_string().contains("fixture.test"), "{s}");

    let inside = GetBrowserStateTool::new(f.engine.clone())
        .invoke(json!({
            "pid": 1, "window_id": 7, "session": SESSION,
            "origin_scope": [SCOPE_FIXTURE],
        }))
        .await;
    assert_eq!(
        structured(&inside)["tabs"][0]["url"],
        "https://fixture.test/"
    );
}

#[tokio::test]
async fn a_read_outside_the_scope_returns_no_page_content() {
    let f = fixture().await;
    let (target, tab) = bind(&f).await;
    let result = GetBrowserStateTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target, "tab_id": tab, "session": SESSION,
            "origin_scope": ["https://other.example"],
            "include_screenshot": true,
        }))
        .await;
    assert_eq!(code(&result), "browser_origin_outside_scope");
    assert!(recorded_calls(&f, "DOM.getDocument").is_empty());
    assert!(recorded_calls(&f, "Page.captureScreenshot").is_empty());
}

#[tokio::test]
async fn a_navigation_during_a_scoped_read_discards_the_read() {
    let f = fixture_with(|st| {
        st.dom_read_navigates_to = Some("https://other.example/private".into());
    })
    .await;
    let (target, tab) = bind(&f).await;
    let result = GetBrowserStateTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target, "tab_id": tab, "session": SESSION,
            "origin_scope": [SCOPE_FIXTURE],
        }))
        .await;
    let s = structured(&result);
    assert_eq!(s["status"], "refused", "{s}");
    assert!(s.get("refs").is_none(), "no page content: {s}");
    // The refs minted by the discarded read are gone.
    assert!(f
        .engine
        .store
        .resolve_ref(SESSION, &target, &tab, "p1:0")
        .is_err());
}

#[tokio::test]
async fn a_cached_continuation_from_an_earlier_document_is_not_returned_under_a_scope() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let (target, tab) = bind(&f).await;
    let first =
        semantic_snapshot_with(&f, &target, &tab, json!({"origin_scope": [SCOPE_FIXTURE]})).await;
    let token = first["snapshot"]["continuation"]
        .as_str()
        .expect("large fixture continuation")
        .to_owned();
    f.state.lock().unwrap().main_loader = "L_MAIN_2".into();
    let continued = semantic_snapshot_with(
        &f,
        &target,
        &tab,
        json!({"continuation": token, "origin_scope": [SCOPE_FIXTURE]}),
    )
    .await;
    assert_eq!(continued["status"], "refused", "{continued}");
    assert!(continued.get("outline").is_none());
}

#[tokio::test]
async fn malformed_scopes_fail_before_any_browser_call() {
    let f = fixture().await;
    let (target, tab) = bind(&f).await;
    let calls = f.state.lock().unwrap().calls.len();
    let result = GetBrowserStateTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target, "tab_id": tab, "session": SESSION,
            "origin_scope": ["https://fixture.test/path"],
        }))
        .await;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(f.state.lock().unwrap().calls.len(), calls);
}

// ── Existing-profile attachments ─────────────────────────────────────────────

async fn attach_existing_profile(f: &Fixture, transport: &str) {
    let prepare = BrowserPrepareTool::new(f.engine.clone())
        .invoke(json!({
            "pid": 1, "window_id": 7, "session": SESSION,
            "_transport_session_id": transport,
            "strategy": { "kind": "existing_profile" }
        }))
        .await;
    assert_eq!(
        structured(&prepare)["status"],
        "ok",
        "{}",
        structured(&prepare)
    );
}

async fn bind_existing(f: &Fixture, transport: &str, scope: Option<&str>) -> ToolResult {
    let mut args = json!({
        "pid": 1, "window_id": 7, "session": SESSION,
        "_transport_session_id": transport,
    });
    if let Some(origin) = scope {
        args["origin_scope"] = json!([origin]);
    }
    GetBrowserStateTool::new(f.engine.clone())
        .invoke(args)
        .await
}

#[tokio::test]
async fn existing_profile_pages_require_a_scope_on_every_page_tool() {
    const TRANSPORT: &str = "transport-scope-required";
    let (f, _) = protected_existing_profile_fixture().await;
    attach_existing_profile(&f, TRANSPORT).await;
    let calls_before_bind = f.state.lock().unwrap().calls.len();

    let unscoped = bind_existing(&f, TRANSPORT, None).await;
    assert_eq!(code(&unscoped), "browser_origin_scope_required");
    assert!(structured(&unscoped).get("tabs").is_none());
    assert_eq!(
        f.state.lock().unwrap().calls.len(),
        calls_before_bind,
        "the tab list is never read without a scope"
    );

    let bound = bind_existing(&f, TRANSPORT, Some(SCOPE_FIXTURE)).await;
    let s = structured(&bound).clone();
    assert_eq!(s["status"], "ok", "{s}");
    let target = s["target_id"].as_str().unwrap().to_owned();
    let tab = s["tabs"][0]["tab_id"].as_str().unwrap().to_owned();

    let unscoped_read = snapshot(&f, &target, &tab).await;
    assert_eq!(
        unscoped_read["refusal"]["code"],
        "browser_origin_scope_required"
    );
    let scoped_read = GetBrowserStateTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target, "tab_id": tab, "session": SESSION,
            "origin_scope": [SCOPE_FIXTURE],
        }))
        .await;
    let scoped_read = structured(&scoped_read).clone();
    assert_eq!(scoped_read["status"], "ok", "{scoped_read}");
    let button = ref_of(&scoped_read, "main", "main-btn");

    let before = page_input_calls(&f);
    let unscoped_click = click(&f, &target, &tab, &button, json!({})).await;
    assert_eq!(code(&unscoped_click), "browser_origin_scope_required");
    let unscoped_navigation = BrowserNavigateTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target, "tab_id": tab, "session": SESSION,
            "url": "https://fixture.test/next",
        }))
        .await;
    assert_eq!(code(&unscoped_navigation), "browser_origin_scope_required");
    assert_eq!(page_input_calls(&f), before, "nothing was sent");

    // A run_steps step or run_script action never reaches these pages, even
    // with a scope.
    let nested = crate::step_pacing::scope(
        crate::step_pacing::StepPacing::Batched,
        click(
            &f,
            &target,
            &tab,
            &button,
            json!({"origin_scope": [SCOPE_FIXTURE]}),
        ),
    )
    .await;
    assert_eq!(code(&nested), "browser_batch_refused");
    let nested_read = crate::step_pacing::scope(
        crate::step_pacing::StepPacing::Single,
        GetBrowserStateTool::new(f.engine.clone()).invoke(json!({
            "target_id": target, "tab_id": tab, "session": SESSION,
            "origin_scope": [SCOPE_FIXTURE],
        })),
    )
    .await;
    assert_eq!(code(&nested_read), "browser_batch_refused");
    assert_eq!(page_input_calls(&f), before, "nothing was sent");

    let scoped_click = click(
        &f,
        &target,
        &tab,
        &button,
        json!({"origin_scope": [SCOPE_FIXTURE]}),
    )
    .await;
    assert!(
        !is_refused(&scoped_click),
        "{:?}",
        scoped_click.structured_content
    );
    crate::session::fire_session_end(TRANSPORT);
}

#[tokio::test]
async fn a_manifest_that_no_longer_lists_the_window_ends_the_attachment() {
    const TRANSPORT: &str = "transport-manifest-narrowed";
    let (f, _) = protected_existing_profile_fixture().await;
    attach_existing_profile(&f, TRANSPORT).await;
    let bound = bind_existing(&f, TRANSPORT, Some(SCOPE_FIXTURE)).await;
    let s = structured(&bound).clone();
    let target = s["target_id"].as_str().unwrap().to_owned();
    let tab = s["tabs"][0]["tab_id"].as_str().unwrap().to_owned();

    let listed = manifest_context(
        &[SCOPE_FIXTURE],
        "    existing_profiles:\n      - {pid: 1, window_id: 7}\n",
    );
    let allowed = read_in(&f, &target, &tab, listed.clone(), true).await;
    assert_eq!(
        structured(&allowed)["status"],
        "ok",
        "{}",
        structured(&allowed)
    );

    // The host narrowed its manifest: the window is gone.
    let narrowed = manifest_context(&[SCOPE_FIXTURE], "");
    let ended = read_in(&f, &target, &tab, narrowed, true).await;
    assert_eq!(
        code(&ended),
        "browser_consent_required",
        "{}",
        structured(&ended)
    );
    // The grant is gone: listing the window again does not revive it.
    let after = read_in(&f, &target, &tab, listed, true).await;
    assert_eq!(code(&after), "browser_consent_required");
    crate::session::fire_session_end(TRANSPORT);
}

#[tokio::test]
async fn a_manifest_narrowed_origin_refuses_at_the_next_read() {
    let f = fixture().await;
    let (target, tab) = bind(&f).await;
    let wide = read_in(
        &f,
        &target,
        &tab,
        manifest_context(&[SCOPE_FIXTURE], ""),
        false,
    )
    .await;
    assert_eq!(structured(&wide)["status"], "ok", "{}", structured(&wide));
    let narrow = read_in(
        &f,
        &target,
        &tab,
        manifest_context(&["https://other.example"], ""),
        false,
    )
    .await;
    assert_eq!(code(&narrow), "browser_origin_outside_scope");
}

// ── Typed refusals through the dispatcher ────────────────────────────────────

#[tokio::test]
async fn the_dispatcher_returns_scope_refusals_before_manifest_admission() {
    let f = fixture().await;
    let mut registry = crate::tool::ToolRegistry::new();
    super::super::tools::register_browser_tools(&f.engine, &mut registry);
    let context = manifest_context(&[SCOPE_FIXTURE], "");
    // Outside both the scope and the manifest: the scope's own code.
    let both = registry
        .invoke_with_context(
            "browser_navigate",
            json!({
                "target_id": "t", "tab_id": "tab", "session": SESSION,
                "url": "https://other.example/",
                "origin_scope": [SCOPE_FIXTURE],
            }),
            context.clone(),
        )
        .await;
    assert_eq!(
        code(&both),
        "browser_origin_outside_scope",
        "{:?}",
        both.structured_content
    );
    // Inside the scope but outside the manifest: today's manifest refusal.
    let manifest_only = registry
        .invoke_with_context(
            "browser_navigate",
            json!({
                "target_id": "t", "tab_id": "tab", "session": SESSION,
                "url": "https://other.example/",
                "origin_scope": ["https://other.example"],
            }),
            context,
        )
        .await;
    assert_eq!(
        manifest_only.structured_content.unwrap()["refusal"]["code"],
        "permission_denied"
    );
    assert!(recorded_calls(&f, "Page.navigate").is_empty());
}
