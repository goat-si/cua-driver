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
