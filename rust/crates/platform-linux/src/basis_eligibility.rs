//! Capture provenance needed for a screenshot coordinate basis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionKind {
    X11,
    Wayland,
}
/// Session identity is independent of whether the native backend was enabled.
pub fn session_kind(
    native_wayland: bool,
    wayland_display: bool,
    session_type_wayland: bool,
) -> SessionKind {
    if native_wayland || wayland_display || session_type_wayland {
        SessionKind::Wayland
    } else {
        SessionKind::X11
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureBackend {
    XShm,
    XGetImage,
    Import,
    RootRegion,
    Other,
}

pub fn capture_eligibility(
    session: SessionKind,
    backend: CaptureBackend,
    proven_xid: bool,
) -> Result<(), &'static str> {
    if session == SessionKind::X11
        && proven_xid
        && matches!(backend, CaptureBackend::XShm | CaptureBackend::XGetImage)
    {
        Ok(())
    } else {
        Err("no_proven_origin")
    }
}

use cua_driver_core::window_basis::{LiveFrame, WindowBasis};
use serde_json::{json, Value};

/// Discard untrusted rows without cancelling the screenshot being captured.
pub fn discard_untrusted_snapshot<S: cua_driver_core::snapshot_store::SnapshotPayload>(
    store: &cua_driver_core::snapshot_store::SnapshotStore<S>,
    pid: i32,
    window_id: u64,
    ticket: Option<u64>,
) -> Option<u32> {
    match ticket {
        Some(ticket) => store.remove_for_capture(pid, window_id, ticket),
        None => store.remove(pid, window_id),
    }
}

#[derive(Clone, Copy, Debug)]
pub enum CapturePath {
    Preview,
    RootRegion,
    WaylandDispatch,
    X11Client(Option<LiveFrame>),
    ImportFallback,
}

#[allow(clippy::too_many_arguments)]
pub fn capture_basis(
    window_id: u64,
    pid: i32,
    session: SessionKind,
    path: CapturePath,
    pre: Option<LiveFrame>,
    post: Option<LiveFrame>,
    native: (u32, u32),
    delivered: (u32, u32),
    captured_at_ms: u64,
) -> Result<WindowBasis, &'static str> {
    let geometry = match (session, path) {
        (SessionKind::X11, CapturePath::X11Client(geometry)) => geometry,
        _ => None,
    };
    cua_driver_core::basis_capture::build_capture_basis(
        window_id,
        pid,
        pre,
        post,
        geometry,
        native,
        delivered,
        captured_at_ms,
    )
}

pub fn response_fields(
    stored: Option<&WindowBasis>,
    capture: Option<&Result<WindowBasis, &'static str>>,
    published: bool,
    screenshot_present: bool,
    observation_only: bool,
) -> Value {
    if let Some(basis) = stored {
        json!({"window_basis": basis.to_json()})
    } else if screenshot_present && !observation_only {
        match cua_driver_core::basis_capture::basis_unavailable_reason(capture, published, false) {
            Some(reason) => json!({"basis_unavailable_reason": reason}),
            None => json!({}),
        }
    } else {
        json!({})
    }
}
