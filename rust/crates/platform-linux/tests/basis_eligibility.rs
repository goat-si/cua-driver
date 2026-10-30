use platform_linux::basis_eligibility::{capture_eligibility, CaptureBackend, SessionKind};

#[test]
fn only_exact_x11_drawable_capture_is_eligible() {
    for backend in [CaptureBackend::XShm, CaptureBackend::XGetImage] {
        assert_eq!(capture_eligibility(SessionKind::X11, backend, true), Ok(()));
        assert_eq!(capture_eligibility(SessionKind::X11, backend, false), Err("no_proven_origin"));
        assert_eq!(capture_eligibility(SessionKind::Wayland, backend, true), Err("no_proven_origin"));
    }
    for backend in [CaptureBackend::Import, CaptureBackend::RootRegion, CaptureBackend::Other] {
        assert_eq!(capture_eligibility(SessionKind::X11, backend, true), Err("no_proven_origin"));
    }
}
