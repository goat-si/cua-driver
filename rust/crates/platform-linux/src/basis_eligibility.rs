//! Capture provenance needed for a screenshot coordinate basis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionKind {
    X11,
    Wayland,
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
