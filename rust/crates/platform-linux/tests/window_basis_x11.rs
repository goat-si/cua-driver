#![cfg(target_os = "linux")]

use anyhow::Result;
use cua_driver_core::basis_capture::build_capture_basis;
use platform_linux::capture::{
    png_dimensions_pub, read_live_frame, resize_png_if_needed, screenshot_window_with_geometry,
};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::wrapper::ConnectionExt as _;

#[test]
#[ignore = "requires an isolated X11 display"]
fn client_capture_basis_tracks_origin_resize_and_downscale() -> Result<()> {
    let (conn, screen) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen].root;
    let window = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        window,
        root,
        50,
        60,
        200,
        120,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new().override_redirect(1),
    )?
    .check()?;
    conn.map_window(window)?.check()?;
    let pre = read_live_frame(window.into())?;
    let (png, geometry) = screenshot_window_with_geometry(window.into())?;
    let post = read_live_frame(window.into())?;
    let native = png_dimensions_pub(&png)?;
    assert_eq!((pre.x, pre.y, pre.width, pre.height), (50, 60, 200, 120));
    assert_eq!(geometry, Some(pre));
    assert_eq!(native, (200, 120));
    let scaled = resize_png_if_needed(&png, 100)?;
    let delivered = png_dimensions_pub(&scaled)?;
    let basis = build_capture_basis(
        window.into(),
        42,
        Some(pre),
        Some(post),
        geometry,
        native,
        delivered,
        123,
    )
    .unwrap();
    assert_eq!(basis.scale_x, 0.5);
    assert_eq!(basis.scale_y, 0.5);
    assert_eq!(basis.check_and_map(&post, 10.0, 15.0).unwrap(), (70, 90));
    println!("window_basis example: {}", basis.to_json());
    conn.configure_window(window, &ConfigureWindowAux::new().x(51))?
        .check()?;
    let moved = read_live_frame(window.into())?;
    let refusal = basis.check(&moved).unwrap_err();
    assert_eq!(
        (refusal.code, refusal.reason),
        ("stale_basis", "window_moved")
    );
    conn.destroy_window(window)?.check()?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an isolated X11 display"]
async fn tool_response_basis_matches_delivered_screenshot() -> Result<()> {
    use serde_json::json;
    let (conn, screen) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen].root;
    let window = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        window,
        root,
        50,
        60,
        200,
        120,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new().override_redirect(1),
    )?
    .check()?;
    let pid_atom = conn.intern_atom(false, b"_NET_WM_PID")?.reply()?.atom;
    conn.change_property32(
        PropMode::REPLACE,
        window,
        pid_atom,
        AtomEnum::CARDINAL,
        &[std::process::id()],
    )?
    .check()?;
    conn.map_window(window)?.check()?;
    let registry = platform_linux::tools::build_registry(false);
    let response = registry
        .invoke(
            "get_window_state",
            json!({
                "pid": std::process::id(), "window_id": window,
                "include_accessibility_tree": false, "max_image_dimension": 100
            }),
        )
        .await;
    assert_ne!(response.is_error, Some(true), "{response:?}");
    let structured = response.structured_content.expect("structured response");
    let basis = &structured["window_basis"];
    assert_eq!(
        (
            basis["x"].as_i64(),
            basis["y"].as_i64(),
            basis["width"].as_u64(),
            basis["height"].as_u64()
        ),
        (Some(50), Some(60), Some(200), Some(120))
    );
    assert_eq!(basis["capture_width"], structured["screenshot_width"]);
    assert_eq!(basis["capture_height"], structured["screenshot_height"]);
    assert_eq!(
        basis["scale_x"].as_f64().unwrap(),
        structured["screenshot_width"].as_f64().unwrap() / 200.0
    );
    assert_eq!(
        basis["scale_y"].as_f64().unwrap(),
        structured["screenshot_height"].as_f64().unwrap() / 120.0
    );
    assert_eq!(basis["snapshot_id"], structured["snapshot_id"]);
    assert!(structured["snapshot_id"]
        .as_str()
        .is_some_and(|id| !id.is_empty()));
    println!("tool response window_basis: {basis}");
    conn.destroy_window(window)?.check()?;
    Ok(())
}
