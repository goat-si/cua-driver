//! Window screenshot via PrintWindow + GDI BitBlt on Windows.
//!
//! `PW_RENDERFULLCONTENT` (0x2) renders the window contents even if it is
//! occluded or off-screen for GDI-backed surfaces. The result is encoded as
//! base64 PNG in memory.
//!
//! ## UWP / DirectComposition fallback (CUA-542)
//!
//! `PrintWindow` doesn't capture DirectComposition-backed surfaces —
//! modern UWP / WinUI3 apps (Calculator, Photos, Settings, Win 11
//! Notepad) render directly to the GPU compositor and have no GDI back
//! buffer for `PrintWindow` to copy from. Result is an all-black image.
//!
//! When the PrintWindow result comes back mostly-black (sentinel for
//! that case), we fall back to a **screen-region BitBlt**: read the
//! window's on-screen bounds via `GetWindowRect`, BitBlt the matching
//! pixels off the desktop DC. This is the same approach the Windows
//! Snipping Tool's "Window" mode uses. Trade-off: only works when the
//! window is actually on-screen and not occluded by another window.
//! For our daemon-driven agent flow that's the common case anyway —
//! the daemon lives in the user's interactive session and the target
//! is typically a visible window the agent just launched.
//!
//! The full proper fix (Windows.Graphics.Capture, which works for
//! occluded / off-screen UWP windows too) is tracked separately on
//! CUA-542; the screen-region fallback covers the same common
//! ground at a fraction of the implementation cost.

#[cfg(target_os = "windows")]
use anyhow::bail;
use anyhow::Result;
#[cfg(target_os = "windows")]
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
#[cfg(target_os = "windows")]
use windows::Win32::Foundation::HWND;
#[cfg(target_os = "windows")]
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, RGBQUAD, SRCCOPY,
};
#[cfg(target_os = "windows")]
use windows::Win32::Graphics::Gdi::{GetWindowDC, ReleaseDC};
#[cfg(target_os = "windows")]
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
#[cfg(target_os = "windows")]
const PW_RENDERFULLCONTENT: PRINT_WINDOW_FLAGS = PRINT_WINDOW_FLAGS(2u32);

mod frame_geometry {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) struct FrameGeometry {
        pub left: i32,
        pub top: i32,
        pub width: i32,
        pub height: i32,
    }

    impl FrameGeometry {
        pub fn from_bounds((left, top, right, bottom): (i32, i32, i32, i32)) -> Option<Self> {
            let width = right.checked_sub(left)?;
            let height = bottom.checked_sub(top)?;
            (width > 0 && height > 0).then_some(Self {
                left,
                top,
                width,
                height,
            })
        }

        pub fn cropped(self, bounds: Option<(i32, i32, i32, i32)>) -> Self {
            let crop = bounds.and_then(|(left, top, right, bottom)| {
                Self::from_bounds((
                    left.checked_add(1)?,
                    top.checked_add(1)?,
                    right.checked_sub(1)?,
                    bottom.checked_sub(1)?,
                ))
            });
            crop.filter(|crop| {
                let x = i64::from(crop.left) - i64::from(self.left);
                let y = i64::from(crop.top) - i64::from(self.top);
                x >= 0
                    && y >= 0
                    && x + i64::from(crop.width) <= i64::from(self.width)
                    && y + i64::from(crop.height) <= i64::from(self.height)
            })
            .unwrap_or(self)
        }

        pub fn local_point(self, x: i32, y: i32) -> Option<(f64, f64)> {
            let x = i64::from(x) - i64::from(self.left);
            let y = i64::from(y) - i64::from(self.top);
            (x >= 0 && y >= 0 && x < i64::from(self.width) && y < i64::from(self.height))
                .then_some((x as f64, y as f64))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::FrameGeometry;

        #[test]
        fn dwm_crop_maps_screen_point_to_actual_frame() {
            let frame = FrameGeometry::from_bounds((-100, 20, 300, 320)).unwrap();
            let crop = frame.cropped(Some((-92, 20, 292, 312)));
            assert_eq!(
                crop,
                FrameGeometry {
                    left: -91,
                    top: 21,
                    width: 382,
                    height: 290
                }
            );
            assert_eq!(crop.local_point(-90, 22), Some((1.0, 1.0)));
            assert_eq!(crop.local_point(291, 22), None);
            assert_eq!(crop.local_point(-92, 22), None);
            assert_eq!(crop.local_point(-90, 311), None);
            assert_eq!(crop.local_point(-90, 20), None);
        }

        #[test]
        fn failed_or_outside_crop_keeps_window_rect_origin() {
            let frame = FrameGeometry::from_bounds((100, -200, 500, 100)).unwrap();
            for bounds in [
                None,
                Some((0, 0, 100, 100)),
                Some((100, -200, 600, 100)),
                Some((100, -200, 101, -199)),
            ] {
                assert_eq!(frame.cropped(bounds), frame);
            }
            assert_eq!(frame.local_point(100, -200), Some((0.0, 0.0)));
            assert_eq!(frame.local_point(499, 99), Some((399.0, 299.0)));
        }

        #[test]
        fn extreme_bounds_and_points_do_not_overflow() {
            assert_eq!(FrameGeometry::from_bounds((i32::MIN, 0, i32::MAX, 1)), None);
            assert_eq!(FrameGeometry::from_bounds((0, 0, 1, 0)), None);
            let frame = FrameGeometry::from_bounds((0, 0, 100, 100)).unwrap();
            assert_eq!(frame.cropped(Some((i32::MAX, 0, i32::MIN, 100))), frame);
            assert_eq!(frame.cropped(Some((i32::MIN, 0, i32::MAX, 100))), frame);
            assert_eq!(frame.local_point(i32::MIN, i32::MAX), None);
        }
    }
}

#[cfg(target_os = "windows")]
use frame_geometry::FrameGeometry;

/// After GetDIBits we have BGRA bytes from PrintWindow. If essentially every
/// pixel is fully-transparent black or fully-opaque black, treat the capture
/// as "PrintWindow didn't render this surface" and let the caller fall back
/// to Windows.Graphics.Capture.
///
/// We sample sparsely (every 64th pixel) so the heuristic is cheap even on
/// 4K windows. The threshold is intentionally aggressive — UWP apps return
/// all-zeros bitmaps, not just dark frames — so legitimate dark UI doesn't
/// trip the fallback.
fn is_mostly_black_bgra(bgra: &[u8]) -> bool {
    if bgra.len() < 16 {
        return true;
    }
    let pixel_count = bgra.len() / 4;
    if pixel_count == 0 {
        return true;
    }
    let stride = (pixel_count / 1024).max(1);
    let mut sampled = 0usize;
    let mut black = 0usize;
    for i in (0..pixel_count).step_by(stride) {
        let off = i * 4;
        // BGRA layout. We consider a pixel "black" when B+G+R == 0,
        // regardless of alpha — that's the all-zero pattern UWP /
        // DirectComposition leaves behind.
        if bgra[off] == 0 && bgra[off + 1] == 0 && bgra[off + 2] == 0 {
            black += 1;
        }
        sampled += 1;
    }
    // > 99.5% of sampled pixels are black → treat as failed render.
    sampled > 0 && (black * 200) >= (sampled * 199)
}

/// Resolve a window capture attempt with fail-closed Windows.Graphics.Capture
/// fallback, never falling back to a desktop/screen-region BitBlt that could
/// leak pixels from an occluding foreground window.
fn resolve_window_capture_fallback<T, F>(primary: Result<T>, wgc_fallback: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    match primary {
        Ok(capture) => Ok(capture),
        Err(primary_error) => {
            if primary_error.to_string().contains("minimized window") {
                return Err(primary_error);
            }
            wgc_fallback().map_err(|wgc_error| {
                anyhow::anyhow!(
                    "{primary_error:#}; Windows.Graphics.Capture fallback failed: {wgc_error:#}"
                )
            })
        }
    }
}

/// Capture a window by HWND, returning raw PNG bytes.
#[cfg(target_os = "windows")]
pub fn screenshot_window_bytes(hwnd: u64) -> Result<Vec<u8>> {
    screenshot_window_bytes_with_occlusion(hwnd).map(|(b, _)| b)
}

/// Capture a window by HWND. Returns (png_bytes, occluded_flag).
///
/// Window capture uses PrintWindow(PW_RENDERFULLCONTENT) and
/// Windows.Graphics.Capture only (both read the target window's own
/// composited surface regardless of z-order) and fails closed if both fail,
/// so occluded_flag is always false.
#[cfg(target_os = "windows")]
pub fn screenshot_window_bytes_with_occlusion(hwnd: u64) -> Result<(Vec<u8>, bool)> {
    screenshot_window_mapped(hwnd).map(|(png, occluded, _)| (png, occluded))
}

/// Capture the final input point against the exact source frame. WGC supplies
/// no proven screen origin, so it cannot produce mapped click evidence yet.
#[cfg(target_os = "windows")]
pub(crate) fn screenshot_window_click_target(
    hwnd: u64,
    screen_x: i32,
    screen_y: i32,
) -> Option<(Vec<u8>, f64, f64)> {
    let (png, occluded, geometry) = screenshot_window_mapped(hwnd).ok()?;
    if occluded {
        return None;
    }
    let (x, y) = geometry?.local_point(screen_x, screen_y)?;
    Some((png, x, y))
}

#[cfg(target_os = "windows")]
fn screenshot_window_mapped(hwnd: u64) -> Result<(Vec<u8>, bool, Option<FrameGeometry>)> {
    resolve_window_capture_fallback(
        unsafe { screenshot_window_bytes_with_occlusion_unsafe(hwnd) },
        || {
            let (pixels, width, height) = crate::wgc::screenshot_window_via_wgc(hwnd)?;
            Ok((
                cua_driver_core::image_utils::encode_bgra_to_png(&pixels, width, height)?,
                false,
                None,
            ))
        },
    )
}

/// Capture a window by HWND, returning (base64_png, width, height).
#[cfg(target_os = "windows")]
pub fn screenshot_window(hwnd: u64) -> Result<(String, u32, u32)> {
    let png_bytes = screenshot_window_bytes(hwnd)?;
    let (w, h) = {
        if png_bytes.len() < 24 {
            bail!("PNG too small");
        }
        let w = u32::from_be_bytes([png_bytes[16], png_bytes[17], png_bytes[18], png_bytes[19]]);
        let h = u32::from_be_bytes([png_bytes[20], png_bytes[21], png_bytes[22], png_bytes[23]]);
        (w, h)
    };
    Ok((BASE64.encode(&png_bytes), w, h))
}

#[cfg(target_os = "windows")]
unsafe fn screenshot_window_bytes_with_occlusion_unsafe(
    hwnd: u64,
) -> Result<(Vec<u8>, bool, Option<FrameGeometry>)> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, IsIconic};

    let hwnd_raw = hwnd;
    let hwnd = HWND(hwnd as *mut _);

    // Bail loudly on minimized (iconic) windows BEFORE attempting any
    // capture path. On Windows, `GetWindowRect` on an iconic HWND returns
    // the off-screen "iconic position" (typically `(-32000, -32000,
    // -31840, -31972)` i.e. w=160, h=28, both positive) and `PrintWindow`
    // paints nothing into the bitmap. The result is a heavily-compressed
    // all-black ~28x160 PNG (~300 bytes) that an upstream agent can't tell
    // apart from a real "blank screen" capture — wasting model turns
    // retrying against a window that's literally minimized to the taskbar.
    //
    // The WGC sibling path at `wgc.rs:58` already short-circuits this case;
    // the GDI/PrintWindow fallback below + the screen-region BitBlt fallback
    // both happily produced the degenerate PNG. Guarding here covers both
    // and matches the WGC error shape so callers can `list_windows` and
    // restore the window before retrying.
    if IsIconic(hwnd).as_bool() {
        bail!(
            "cannot capture minimized window 0x{hwnd_raw:x}: it has no \
             rendered content. Call bring_to_front with this window_id to \
             restore it first. The PrintWindow GDI path and the screen-region \
             BitBlt fallback both return an all-black bitmap for iconic \
             windows."
        );
    }

    // CUA-542 routing: for known XAML / WinUI3 / UWP targets, the
    // PrintWindow GDI path returns black (DirectComposition isn't in
    // the GDI back-buffer). We try Windows.Graphics.Capture (WGC)
    // first — that's the only API that returns the target's actual
    // composited pixels even when it's occluded by another window
    // (the Calculator-behind-terminal regression captured in the
    // autonomous-test journal). On WGC failure, fall through to PrintWindow
    // (which fails closed below if the surface is mostly-black).
    if crate::input::is_xaml_host_hwnd(hwnd_raw) {
        match crate::wgc::screenshot_window_via_wgc(hwnd_raw) {
            Ok((pixels, w, h)) => {
                return Ok((
                    cua_driver_core::image_utils::encode_bgra_to_png(&pixels, w, h)?,
                    false, // WGC reads target's own pixels — never occluded by definition
                    None,
                ));
            }
            Err(e) => {
                tracing::warn!(
                    target: "cua-driver",
                    "screenshot: WGC failed for XAML target hwnd 0x{hwnd_raw:x}: {e}; \
                     falling back to PrintWindow."
                );
            }
        }
    }

    // Size the capture buffer to the WHOLE window (GetWindowRect), not
    // just the client area (GetClientRect). PrintWindow draws the entire
    // window at 1:1 starting at (0,0) — if the buffer is sized to the
    // client area only, anything in the non-client region clips. The
    // surprising case is VCL/SAL dialogs (LibreOffice's "Document
    // Recovery", any of its Confirmation modals): VCL puts the bottom
    // button strip OUTSIDE the standard Win32 client area, so a
    // client-sized buffer loses the Save/Cancel/OK row at the bottom.
    // Window-sized buffer captures title bar + body + non-client trim
    // correctly.
    //
    // Under Per-Monitor V2 DPI awareness, GetWindowRect returns PHYSICAL
    // pixels — the same unit GetWindowDC and PrintWindow/BitBlt work in.
    // Use the dimensions as-is; scaling by DPI/96 would allocate an
    // oversized bitmap with the content in its top-left corner and a
    // black margin around it (issue #1879). It also kept the
    // DWMWA_EXTENDED_FRAME_BOUNDS crop below (physical pixels) from
    // matching the bitmap.
    let mut win_rect = RECT::default();
    GetWindowRect(hwnd, &mut win_rect)?;
    let geometry =
        FrameGeometry::from_bounds((win_rect.left, win_rect.top, win_rect.right, win_rect.bottom))
            .ok_or_else(|| anyhow::anyhow!("Window has invalid bounds"))?;
    let w = geometry.width;
    let h = geometry.height;

    let screen_dc = GetWindowDC(hwnd);
    let mem_dc = CreateCompatibleDC(screen_dc);
    let bitmap = CreateCompatibleBitmap(screen_dc, w, h);
    let old_bitmap = SelectObject(mem_dc, bitmap);

    let pw_ok = PrintWindow(hwnd, mem_dc, PW_RENDERFULLCONTENT);
    if !pw_ok.as_bool() {
        SelectObject(mem_dc, old_bitmap);
        let _ = DeleteObject(bitmap);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(hwnd, screen_dc);
        bail!("PrintWindow(PW_RENDERFULLCONTENT) failed for hwnd 0x{hwnd_raw:x}");
    }

    // Compute the DWM-extended-frame bounds. On Win10+ the OS draws an
    // invisible drop-shadow margin around every top-level window that
    // GetWindowRect counts in its dimensions but PrintWindow does NOT
    // actually paint into — leaving a black trim around the body in the
    // captured bitmap (except the title bar, which spans the full width
    // and so doesn't pick up the artifact). DWMWA_EXTENDED_FRAME_BOUNDS
    // returns the rectangle WITHOUT the shadow margin. We compute the
    // offsets relative to GetWindowRect and crop the bitmap below.
    //
    // Best-effort: if the DWM call fails (e.g. very old Windows, hooked
    // by some shell extension), we keep the full-window bitmap as-is —
    // user sees a small dark border but no clipping.
    let dwm_rect: Option<RECT> = {
        use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
        let mut r = RECT::default();
        let hr = DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut _ as *mut _,
            std::mem::size_of::<RECT>() as u32,
        );
        hr.ok().map(|_| r)
    };

    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: (w * h * 4) as u32,
            ..Default::default()
        },
        bmiColors: [RGBQUAD::default(); 1],
    };

    let pixel_count = (w * h) as usize;
    let mut pixels = vec![0u8; pixel_count * 4];
    let ok = GetDIBits(
        mem_dc,
        bitmap,
        0,
        h as u32,
        Some(pixels.as_mut_ptr() as *mut _),
        &mut bmi,
        DIB_RGB_COLORS,
    );

    SelectObject(mem_dc, old_bitmap);
    let _ = DeleteObject(bitmap);
    let _ = DeleteDC(mem_dc);
    ReleaseDC(hwnd, screen_dc);

    if ok == 0 {
        bail!("GetDIBits returned 0");
    }

    // Crop the bitmap to the DWM extended-frame bounds (computed above)
    // to remove the invisible-shadow margin PrintWindow doesn't paint.
    // Skipped on the DWM-failed branch — caller sees a thin dark trim
    // around the body but the body itself is intact.
    //
    // INSET: DWMWA_EXTENDED_FRAME_BOUNDS reports the rect WITHOUT the
    // invisible shadow margin, but Win11 dialogs paint a 1-2 px dark
    // stroke at the rounded-corner edge that ends up at the very edge
    // of the DWM-extended-frame rect. The visual artifact is a thin
    // black hairline along (typically) the bottom or right edge of the
    // captured bitmap. A 1-px inset on each side removes the hairline
    // without losing actual UI content — anything that close to the
    // edge is window-frame chrome, not content.
    let cropped_geometry =
        geometry.cropped(dwm_rect.map(|dwm| (dwm.left, dwm.top, dwm.right, dwm.bottom)));
    let (pixels, w, h) = if cropped_geometry != geometry {
        let off_x = cropped_geometry.left - geometry.left;
        let off_y = cropped_geometry.top - geometry.top;
        let crop_w = cropped_geometry.width;
        let crop_h = cropped_geometry.height;
        let stride_full = (w * 4) as usize;
        let stride_crop = (crop_w * 4) as usize;
        let mut cropped = vec![0u8; (crop_w * crop_h * 4) as usize];
        for row in 0..crop_h as usize {
            let src_row = (off_y as usize + row) * stride_full + (off_x as usize) * 4;
            let dst_row = row * stride_crop;
            cropped[dst_row..dst_row + stride_crop]
                .copy_from_slice(&pixels[src_row..src_row + stride_crop]);
        }
        (cropped, crop_w, crop_h)
    } else {
        (pixels, w, h)
    };

    // CUA-542: detect the all-black bitmap PrintWindow returns for
    // DirectComposition-backed UWP / WinUI3 surfaces and recover via WGC
    // (occlusion-immune). If WGC also fails, fail closed instead of falling
    // back to a screen-region BitBlt that would capture an occluding window.
    let mostly_black = is_mostly_black_bgra(&pixels);
    if mostly_black {
        match crate::wgc::screenshot_window_via_wgc(hwnd_raw) {
            Ok((alt_pixels, w, h)) => {
                return Ok((
                    cua_driver_core::image_utils::encode_bgra_to_png(&alt_pixels, w, h)?,
                    false,
                    None,
                ));
            }
            // The caller retries WGC and reports its error alongside this cause.
            Err(_) => {
                bail!("PrintWindow returned a mostly-black bitmap for hwnd 0x{hwnd_raw:x}");
            }
        }
    }

    // BGRA → PNG via the shared `image_utils::encode_bgra_to_png`
    // helper (extracted from this file 2026-05; was a hand-rolled
    // uncompressed-PNG path that produced ~5x larger output. The
    // `image` crate's encoder is already a workspace dep so the
    // smaller output is free).
    // PrintWindow itself reads from the target's own DC, so the bitmap
    // we return here is the target's pixels even when occluded — no
    // occluded warning needed on this path.
    Ok((
        cua_driver_core::image_utils::encode_bgra_to_png(&pixels, w as u32, h as u32)?,
        false,
        (!mostly_black).then_some(cropped_geometry),
    ))
}

// NOTE: previously this module carried a hand-rolled
// `write_uncompressed_png` + `write_png_chunk` + `zlib_store` +
// `adler32` + `crc32_ieee` (~110 lines) plus a local
// `encode_bgra_to_png` that used them. All of that is replaced by
// `cua_driver_core::image_utils::encode_bgra_to_png` which goes through
// the `image` crate's PNG encoder — already a workspace dep,
// produces ~5x smaller files than the uncompressed-store path.
//
// Same extraction applies to the four pub helpers below
// (`png_bytes_to_jpeg`, `resize_png_if_needed`, `crosshair_png_bytes`,
// `png_dimensions_pub`). They're now thin re-exports of the shared
// `cua_driver_core::image_utils::*` so all three platform crates call the
// same code. See `CUA_DRIVER_RS_DEDUP_AUDIT.md` for the full audit.

/// Capture the primary display (full screen), returning raw PNG bytes.
#[cfg(target_os = "windows")]
pub fn screenshot_display_bytes() -> Result<Vec<u8>> {
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
        // Under Per-Monitor V2 DPI awareness, GetSystemMetrics returns
        // PHYSICAL pixels — the same unit BitBlt captures in. Scaling by
        // DPI/96 would allocate an oversized bitmap with black margins
        // (issue #1879).
        let w = GetSystemMetrics(SM_CXSCREEN);
        let h = GetSystemMetrics(SM_CYSCREEN);
        if w <= 0 || h <= 0 {
            bail!("Could not get screen metrics");
        }
        let screen_dc = GetDC(HWND::default());
        if screen_dc.is_invalid() {
            bail!("GetDC(NULL) returned an invalid desktop DC");
        }
        let mem_dc = CreateCompatibleDC(screen_dc);
        if mem_dc.is_invalid() {
            ReleaseDC(HWND::default(), screen_dc);
            bail!("CreateCompatibleDC returned an invalid memory DC");
        }
        let bitmap = CreateCompatibleBitmap(screen_dc, w, h);
        if bitmap.is_invalid() {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(HWND::default(), screen_dc);
            bail!("CreateCompatibleBitmap returned an invalid bitmap");
        }
        let old_bitmap = SelectObject(mem_dc, bitmap);
        if old_bitmap.is_invalid() {
            let _ = DeleteObject(bitmap);
            let _ = DeleteDC(mem_dc);
            ReleaseDC(HWND::default(), screen_dc);
            bail!("SelectObject failed to select the capture bitmap");
        }
        let blt_result = BitBlt(mem_dc, 0, 0, w, h, screen_dc, 0, 0, SRCCOPY);
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                biSizeImage: (w * h * 4) as u32,
                ..Default::default()
            },
            bmiColors: [RGBQUAD::default(); 1],
        };
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        let ok = GetDIBits(
            mem_dc,
            bitmap,
            0,
            h as u32,
            Some(pixels.as_mut_ptr() as *mut _),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        SelectObject(mem_dc, old_bitmap);
        let _ = DeleteObject(bitmap);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(HWND::default(), screen_dc);
        if let Err(error) = blt_result {
            // A locked PC or a disconnected Remote Desktop session: say which.
            if let Some(why) = crate::diagnostics::desktop_unavailable() {
                bail!("{} (BitBlt: {error})", why.message());
            }
            bail!("desktop capture: BitBlt failed: {error}");
        }
        if ok == 0 {
            bail!("GetDIBits returned 0");
        }
        cua_driver_core::image_utils::encode_bgra_to_png(&pixels, w as u32, h as u32)
    }
}

/// Capture the primary display for the agent, with the Driver's own cursor
/// overlay kept out of the pixels, and report how that went.
#[cfg(target_os = "windows")]
pub fn screenshot_display_bytes_excluding_overlay() -> Result<(
    Vec<u8>,
    cursor_overlay::capture_exclusion::AgentOverlayCapture,
)> {
    use cursor_overlay::capture_exclusion::{capture_excluding_overlays, ResidualCheck};
    capture_excluding_overlays(&crate::overlay::CaptureExcluder, |_| {
        Ok((screenshot_display_bytes()?, ResidualCheck::Clean))
    })
}

/// Capture primary display, returning (base64_png, width, height).
#[cfg(target_os = "windows")]
pub fn screenshot_display() -> Result<(String, u32, u32)> {
    let png_bytes = screenshot_display_bytes()?;
    let (w, h) = cua_driver_core::image_utils::png_dimensions(&png_bytes)?;
    Ok((BASE64.encode(&png_bytes), w, h))
}

// PNG/JPEG/resize/crosshair helpers — re-exports of the shared
// `cua_driver_core::image_utils` module. The previous file-local copies were
// near-identical to the macOS and Linux versions; the dedup-audit
// (2026-05) moved them all to one place.

/// Convert PNG bytes to JPEG at the given quality (1–95).
pub fn png_bytes_to_jpeg(png_bytes: &[u8], quality: u8) -> Result<Vec<u8>> {
    cua_driver_core::image_utils::png_bytes_to_jpeg(png_bytes, quality)
}

/// Downscale `png_bytes` so neither dimension exceeds `max_dim`.
/// If `max_dim == 0` or the image already fits, returns a copy of the
/// original bytes unchanged.
pub fn resize_png_if_needed(png_bytes: &[u8], max_dim: u32) -> Result<Vec<u8>> {
    cua_driver_core::image_utils::resize_png_if_needed(png_bytes, max_dim)
}

/// Draw a red crosshair at pixel (cx, cy) on a PNG image and return
/// modified PNG bytes. Used by recording's click-marker callback to
/// produce click.png.
pub fn crosshair_png_bytes(png_bytes: &[u8], cx: f64, cy: f64) -> Result<Vec<u8>> {
    cua_driver_core::image_utils::crosshair_png_bytes(png_bytes, cx, cy)
}

/// Parse width and height from a PNG IHDR chunk.
///
/// Suffixed `_pub` because an older private `png_dimensions` predated
/// the `_pub` export; the public alias is what callers use today.
pub fn png_dimensions_pub(data: &[u8]) -> Result<(u32, u32)> {
    cua_driver_core::image_utils::png_dimensions(data)
}

#[cfg(test)]
mod capture_fallback_tests {
    use super::{is_mostly_black_bgra, resolve_window_capture_fallback};

    #[test]
    fn mostly_black_detector_flags_all_zero_bgra_and_keeps_lit_frames() {
        let black = vec![0u8; 64 * 64 * 4];
        assert!(is_mostly_black_bgra(&black));

        let mut lit = vec![0u8; 64 * 64 * 4];
        for pixel in lit.chunks_exact_mut(4).take(128) {
            pixel[0] = 0x40;
            pixel[1] = 0x80;
            pixel[2] = 0xC0;
            pixel[3] = 0xFF;
        }
        assert!(!is_mostly_black_bgra(&lit));
    }

    #[test]
    fn window_capture_fallback_fails_closed_when_primary_and_wgc_fail() {
        let err = resolve_window_capture_fallback::<(Vec<u8>, bool), _>(
            Err(anyhow::anyhow!(
                "PrintWindow(PW_RENDERFULLCONTENT) failed for hwnd 0x1234"
            )),
            || Err(anyhow::anyhow!("WGC unavailable")),
        )
        .unwrap_err();
        for msg in [err.to_string(), format!("{err:#}")] {
            assert!(msg.contains("PrintWindow(PW_RENDERFULLCONTENT) failed"));
            assert!(msg.contains("Windows.Graphics.Capture fallback failed: WGC unavailable"));
            assert!(!msg.contains("screen-region"));
            assert!(!msg.contains("desktop-region"));
        }
    }

    #[test]
    fn window_capture_fallback_short_circuits_minimized_windows_without_calling_wgc() {
        let mut wgc_called = false;
        let err = resolve_window_capture_fallback::<(Vec<u8>, bool), _>(
            Err(anyhow::anyhow!("cannot capture minimized window 0x1234")),
            || {
                wgc_called = true;
                Ok((vec![1, 2, 3], false))
            },
        )
        .unwrap_err();
        assert!(!wgc_called);
        assert!(err.to_string().contains("minimized window"));
    }
}
