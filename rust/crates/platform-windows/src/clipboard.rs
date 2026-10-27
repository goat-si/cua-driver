use std::path::Path;
#[cfg(target_os = "windows")]
use std::sync::Mutex;

#[cfg(target_os = "windows")]
use clipboard_rs::{common::RustImage, Clipboard, ClipboardContext, ContentFormat};
#[cfg(target_os = "windows")]
use cua_driver_core::clipboard::ClipboardBackend;

/// Windows clipboard format that instructs clipboard monitors and the OS
/// clipboard service to ignore the current clipboard update.
pub const EXCLUDE_CLIPBOARD_MONITOR_FORMAT: &str = "ExcludeClipboardContentFromMonitorProcessing";

/// Windows clipboard format (DWORD 0) that prevents the item from appearing
/// in Win+V clipboard history.
pub const CAN_INCLUDE_IN_CLIPBOARD_HISTORY_FORMAT: &str = "CanIncludeInClipboardHistory";

/// Windows clipboard format (DWORD 0) that prevents the item from syncing to
/// cloud clipboard across devices.
pub const CAN_UPLOAD_TO_CLOUD_CLIPBOARD_FORMAT: &str = "CanUploadToCloudClipboard";

/// Serialized DWORD(0) payload ([0, 0, 0, 0]) used by Windows clipboard
/// exclusion formats.
pub const CLIPBOARD_EXCLUSION_DWORD_ZERO: [u8; 4] = 0u32.to_le_bytes();

/// Return the ordered (format_name, dword_payload) pairs required to keep
/// agent-written clipboard data out of Windows clipboard history and cloud sync.
pub fn exclusion_format_payloads() -> [(&'static str, [u8; 4]); 3] {
    [
        (
            EXCLUDE_CLIPBOARD_MONITOR_FORMAT,
            CLIPBOARD_EXCLUSION_DWORD_ZERO,
        ),
        (
            CAN_INCLUDE_IN_CLIPBOARD_HISTORY_FORMAT,
            CLIPBOARD_EXCLUSION_DWORD_ZERO,
        ),
        (
            CAN_UPLOAD_TO_CLOUD_CLIPBOARD_FORMAT,
            CLIPBOARD_EXCLUSION_DWORD_ZERO,
        ),
    ]
}

/// Abstraction over registering and setting custom clipboard formats inside an
/// open clipboard transaction so format registration and payload writing can be
/// unit-tested on any host OS.
pub trait ClipboardExclusionSink {
    fn register_format(&mut self, name: &str) -> Result<u32, String>;
    fn set_format_dword(&mut self, format_id: u32, payload: [u8; 4]) -> Result<(), String>;
}

/// Register and write all three Windows clipboard exclusion formats into sink.
pub fn apply_clipboard_exclusion_formats<S: ClipboardExclusionSink>(
    sink: &mut S,
) -> Result<[u32; 3], String> {
    let specs = exclusion_format_payloads();
    let mut ids = [0u32; 3];
    for (idx, (name, payload)) in specs.into_iter().enumerate() {
        let format_id = sink.register_format(name)?;
        if format_id == 0 {
            return Err(format!("RegisterClipboardFormatW returned 0 for {name}"));
        }
        sink.set_format_dword(format_id, payload)?;
        ids[idx] = format_id;
    }
    Ok(ids)
}

/// `CF_DIBV5` payload for an RGBA8 image: a `BITMAPV5HEADER` with
/// BGRA bit fields followed by bottom-up 32-bit rows. Windows synthesizes
/// `CF_DIB` and `CF_BITMAP` from it for readers that want those.
pub fn dibv5_from_rgba(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let row = (width as usize)
        .checked_mul(4)
        .ok_or("image is too wide for the clipboard")?;
    let size = row
        .checked_mul(height as usize)
        .filter(|size| *size == rgba.len() && *size > 0 && *size <= u32::MAX as usize)
        .ok_or("image pixels do not match its size")?;
    let width = i32::try_from(width).map_err(|_| "image is too wide for the clipboard")?;
    let height = i32::try_from(height).map_err(|_| "image is too tall for the clipboard")?;
    let mut dib = Vec::with_capacity(124 + size);
    dib.extend_from_slice(&124u32.to_le_bytes()); // bV5Size
    dib.extend_from_slice(&width.to_le_bytes());
    dib.extend_from_slice(&height.to_le_bytes()); // positive: bottom-up rows
    dib.extend_from_slice(&1u16.to_le_bytes()); // planes
    dib.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
    dib.extend_from_slice(&3u32.to_le_bytes()); // BI_BITFIELDS
    dib.extend_from_slice(&(size as u32).to_le_bytes());
    dib.extend_from_slice(&[0; 16]); // pels per meter, colors used/important
    for mask in [0x00FF_0000u32, 0x0000_FF00, 0x0000_00FF, 0xFF00_0000] {
        dib.extend_from_slice(&mask.to_le_bytes()); // red, green, blue, alpha
    }
    dib.extend_from_slice(&0x7352_4742u32.to_le_bytes()); // LCS_sRGB
    dib.extend_from_slice(&[0; 48]); // endpoints and gamma
    dib.extend_from_slice(&4u32.to_le_bytes()); // LCS_GM_IMAGES
    dib.extend_from_slice(&[0; 12]); // profile data, profile size, reserved
    for line in rgba.chunks_exact(row).rev() {
        for pixel in line.chunks_exact(4) {
            dib.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
    }
    Ok(dib)
}

/// `CF_HDROP` payload: a `DROPFILES` header (wide names) followed by each
/// path as NUL-terminated UTF-16 and a final NUL.
pub fn hdrop_from_paths(paths: &[&str]) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&20u32.to_le_bytes()); // pFiles: names follow the header
    payload.extend_from_slice(&[0; 12]); // pt.x, pt.y, fNC
    payload.extend_from_slice(&1u32.to_le_bytes()); // fWide
    for path in paths {
        for unit in path.encode_utf16().chain(std::iter::once(0)) {
            payload.extend_from_slice(&unit.to_le_bytes());
        }
    }
    payload.extend_from_slice(&0u16.to_le_bytes());
    payload
}

#[cfg(target_os = "windows")]
pub struct WindowsClipboard {
    context: Result<Mutex<ClipboardContext>, String>,
}

#[cfg(target_os = "windows")]
impl WindowsClipboard {
    pub fn new() -> Self {
        Self {
            context: ClipboardContext::new()
                .map(Mutex::new)
                .map_err(|error| error.to_string()),
        }
    }

    fn context(&self) -> Result<std::sync::MutexGuard<'_, ClipboardContext>, String> {
        self.context
            .as_ref()
            .map_err(Clone::clone)?
            .lock()
            .map_err(|_| "clipboard lock was poisoned".to_owned())
    }
}

fn absolute_existing_file(path: &str) -> Result<String, String> {
    let path = Path::new(path);
    if !path.is_absolute() {
        return Err("clipboard file paths must be absolute".into());
    }
    let canonical = path.canonicalize().map_err(|error| error.to_string())?;
    if !canonical.is_file() {
        return Err("clipboard path must identify an existing file".into());
    }
    Ok(canonical.to_string_lossy().into_owned())
}

#[cfg(target_os = "windows")]
mod win32_exclusion {
    use super::{apply_clipboard_exclusion_formats, ClipboardExclusionSink};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{GlobalFree, HANDLE, HWND};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

    const CF_UNICODETEXT_ID: u32 = 13;

    struct OpenClipboardGuard;

    impl OpenClipboardGuard {
        fn open() -> Result<Self, String> {
            for attempt in 0..5 {
                if unsafe { OpenClipboard(HWND::default()) }.is_ok() {
                    return Ok(Self);
                }
                if attempt < 4 {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
            Err("OpenClipboard failed".to_owned())
        }
    }

    impl Drop for OpenClipboardGuard {
        fn drop(&mut self) {
            let _ = unsafe { CloseClipboard() };
        }
    }

    unsafe fn set_global_bytes(format_id: u32, bytes: &[u8]) -> Result<(), String> {
        let hmem = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1))
            .map_err(|e| format!("GlobalAlloc failed for format {format_id}: {e}"))?;
        let ptr = GlobalLock(hmem);
        if ptr.is_null() {
            let _ = GlobalFree(hmem);
            return Err(format!("GlobalLock returned null for format {format_id}"));
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
        let _ = GlobalUnlock(hmem);
        if let Err(err) = SetClipboardData(format_id, HANDLE(hmem.0)) {
            let _ = GlobalFree(hmem);
            return Err(format!(
                "SetClipboardData failed for format {format_id}: {err}"
            ));
        }
        Ok(())
    }

    struct Win32ExclusionSink;

    impl ClipboardExclusionSink for Win32ExclusionSink {
        fn register_format(&mut self, name: &str) -> Result<u32, String> {
            let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            let id = unsafe { RegisterClipboardFormatW(PCWSTR(wide.as_ptr())) };
            if id == 0 {
                Err(format!("RegisterClipboardFormatW({name}) returned 0"))
            } else {
                Ok(id)
            }
        }

        fn set_format_dword(&mut self, format_id: u32, payload: [u8; 4]) -> Result<(), String> {
            unsafe { set_global_bytes(format_id, &payload) }
        }
    }

    pub fn write_text_with_exclusion(text: &str) -> Result<(), String> {
        let _guard = OpenClipboardGuard::open()?;
        unsafe {
            EmptyClipboard().map_err(|e| format!("EmptyClipboard failed: {e}"))?;
            let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            let byte_len = utf16.len() * std::mem::size_of::<u16>();
            let bytes = std::slice::from_raw_parts(utf16.as_ptr() as *const u8, byte_len);
            set_global_bytes(CF_UNICODETEXT_ID, bytes)?;
        }
        let mut sink = Win32ExclusionSink;
        apply_clipboard_exclusion_formats(&mut sink)?;
        Ok(())
    }

    const CF_HDROP_ID: u32 = 15;
    const CF_DIBV5_ID: u32 = 17;

    /// Replace the clipboard with `formats` and the exclusion formats in one
    /// open/close. Clipboard history and cloud sync read the clipboard when it
    /// closes, so the exclusion must be there at the first close.
    fn write_with_exclusion(formats: &[(u32, &[u8])]) -> Result<(), String> {
        let _guard = OpenClipboardGuard::open()?;
        unsafe {
            EmptyClipboard().map_err(|e| format!("EmptyClipboard failed: {e}"))?;
            for (format_id, bytes) in formats {
                set_global_bytes(*format_id, bytes)?;
            }
        }
        let mut sink = Win32ExclusionSink;
        apply_clipboard_exclusion_formats(&mut sink)?;
        Ok(())
    }

    pub fn write_image_with_exclusion(png: &[u8], dibv5: &[u8]) -> Result<(), String> {
        let png_format = Win32ExclusionSink.register_format("PNG")?;
        write_with_exclusion(&[(png_format, png), (CF_DIBV5_ID, dibv5)])
    }

    pub fn write_files_with_exclusion(hdrop: &[u8]) -> Result<(), String> {
        write_with_exclusion(&[(CF_HDROP_ID, hdrop)])
    }
}

#[cfg(target_os = "windows")]
impl ClipboardBackend for WindowsClipboard {
    fn available_formats(&self) -> Result<Vec<String>, String> {
        self.context()?
            .available_formats()
            .map_err(|e| e.to_string())
    }

    fn read_text(&self) -> Result<Option<String>, String> {
        let context = self.context()?;
        if context.has(ContentFormat::Text) {
            context.get_text().map(Some).map_err(|e| e.to_string())
        } else {
            Ok(None)
        }
    }

    fn write_text(&self, text: String) -> Result<(), String> {
        let _guard = self.context()?;
        win32_exclusion::write_text_with_exclusion(&text)
    }

    fn write_image(&self, absolute_path: &str) -> Result<(), String> {
        let path = absolute_existing_file(absolute_path)?;
        let image = clipboard_rs::RustImageData::from_path(&path).map_err(|e| e.to_string())?;
        let png = image.to_png().map_err(|e| e.to_string())?;
        let rgba = image.to_rgba8().map_err(|e| e.to_string())?;
        let dibv5 = dibv5_from_rgba(rgba.width(), rgba.height(), rgba.as_raw())?;
        let _guard = self.context()?;
        win32_exclusion::write_image_with_exclusion(png.get_bytes(), &dibv5)
    }

    fn write_file_url(&self, absolute_path: &str) -> Result<(), String> {
        let path = absolute_existing_file(absolute_path)?;
        let _guard = self.context()?;
        win32_exclusion::write_files_with_exclusion(&hdrop_from_paths(&[&path]))
    }
}

#[cfg(test)]
mod tests {
    /// The native clipboard tests share the system clipboard, and one counts
    /// its update notifications, so they must not run at the same time.
    #[cfg(target_os = "windows")]
    static NATIVE_CLIPBOARD_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    use super::*;

    #[derive(Default)]
    struct MockExclusionSink {
        registered: Vec<String>,
        written: Vec<(u32, [u8; 4])>,
        fail_on_register: Option<&'static str>,
        fail_on_set: Option<u32>,
    }

    impl ClipboardExclusionSink for MockExclusionSink {
        fn register_format(&mut self, name: &str) -> Result<u32, String> {
            if self.fail_on_register == Some(name) {
                return Ok(0);
            }
            self.registered.push(name.to_owned());
            Ok(0xC000 + self.registered.len() as u32)
        }

        fn set_format_dword(&mut self, format_id: u32, payload: [u8; 4]) -> Result<(), String> {
            if self.fail_on_set == Some(format_id) {
                return Err(format!("mock SetClipboardData failure for {format_id}"));
            }
            self.written.push((format_id, payload));
            Ok(())
        }
    }

    #[test]
    fn exclusion_formats_register_and_write_dword_zero_payloads() {
        let mut sink = MockExclusionSink::default();
        let ids = apply_clipboard_exclusion_formats(&mut sink).unwrap();
        assert_eq!(ids, [0xC001, 0xC002, 0xC003]);
        assert_eq!(
            sink.registered,
            vec![
                "ExcludeClipboardContentFromMonitorProcessing".to_owned(),
                "CanIncludeInClipboardHistory".to_owned(),
                "CanUploadToCloudClipboard".to_owned(),
            ]
        );
        assert_eq!(
            sink.written,
            vec![
                (0xC001, [0, 0, 0, 0]),
                (0xC002, [0, 0, 0, 0]),
                (0xC003, [0, 0, 0, 0]),
            ]
        );
    }

    #[test]
    fn exclusion_formats_surface_registration_and_write_errors() {
        let mut bad_reg = MockExclusionSink {
            fail_on_register: Some(CAN_INCLUDE_IN_CLIPBOARD_HISTORY_FORMAT),
            ..Default::default()
        };
        assert!(apply_clipboard_exclusion_formats(&mut bad_reg)
            .unwrap_err()
            .contains("CanIncludeInClipboardHistory"));

        let mut bad_set = MockExclusionSink {
            fail_on_set: Some(0xC003),
            ..Default::default()
        };
        assert!(apply_clipboard_exclusion_formats(&mut bad_set)
            .unwrap_err()
            .contains("49155"));
    }

    #[test]
    fn rejects_relative_local_paths_before_clipboard_access() {
        assert!(absolute_existing_file("relative.txt")
            .unwrap_err()
            .contains("absolute"));
        assert!(absolute_existing_file("relative.png")
            .unwrap_err()
            .contains("absolute"));
    }

    #[test]
    fn dibv5_payload_is_a_v5_header_with_bottom_up_bgra_rows() {
        // 1x2: top pixel red, bottom pixel half-transparent blue.
        let dib = dibv5_from_rgba(1, 2, &[255, 0, 0, 255, 0, 0, 255, 128]).unwrap();
        let u32_at =
            |offset: usize| u32::from_le_bytes(dib[offset..offset + 4].try_into().unwrap());
        assert_eq!(dib.len(), 124 + 8);
        assert_eq!(u32_at(0), 124);
        assert_eq!((u32_at(4), u32_at(8)), (1, 2));
        assert_eq!(u32::from(u16::from_le_bytes([dib[14], dib[15]])), 32);
        assert_eq!((u32_at(16), u32_at(20)), (3, 8));
        assert_eq!(
            [u32_at(40), u32_at(44), u32_at(48), u32_at(52)],
            [0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0xFF00_0000]
        );
        assert_eq!(u32_at(56), 0x7352_4742);
        assert_eq!(&dib[124..], &[255, 0, 0, 128, 0, 0, 255, 255]);
        assert!(dibv5_from_rgba(2, 2, &[0; 4]).is_err());
        assert!(dibv5_from_rgba(0, 0, &[]).is_err());
    }

    #[test]
    fn hdrop_payload_lists_wide_paths_after_the_dropfiles_header() {
        let payload = hdrop_from_paths(&[r"C:\a.txt"]);
        assert_eq!(&payload[..4], &20u32.to_le_bytes());
        assert_eq!(&payload[16..20], &1u32.to_le_bytes());
        let names: Vec<u16> = payload[20..]
            .chunks_exact(2)
            .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
            .collect();
        let expected: Vec<u16> = r"C:\a.txt".encode_utf16().chain([0, 0]).collect();
        assert_eq!(names, expected);
    }

    /// Clipboard history and cloud sync read the clipboard when its writer
    /// closes it. An image or file write must therefore close the clipboard
    /// once, with the exclusion formats already present. It used to write the
    /// data, close, then reopen to add them.
    #[cfg(target_os = "windows")]
    #[test]
    fn native_image_and_file_writes_close_the_clipboard_once_with_exclusions() {
        use std::cell::RefCell;
        use windows::core::{w, PCWSTR};
        use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
        use windows::Win32::System::DataExchange::{
            AddClipboardFormatListener, IsClipboardFormatAvailable, RegisterClipboardFormatW,
            RemoveClipboardFormatListener,
        };
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, PeekMessageW,
            RegisterClassW, UnregisterClassW, HWND_MESSAGE, MSG, PM_REMOVE, WINDOW_EX_STYLE,
            WINDOW_STYLE, WM_CLIPBOARDUPDATE, WNDCLASSW,
        };

        if std::env::var_os("CUA_TEST_ALLOW_CLIPBOARD").as_deref()
            != Some(std::ffi::OsStr::new("1"))
        {
            return;
        }
        let _serial = NATIVE_CLIPBOARD_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        thread_local! {
            // One entry per clipboard update: whether the history exclusion
            // format was on the clipboard when the update was delivered.
            static UPDATES: RefCell<Vec<bool>> = const { RefCell::new(Vec::new()) };
        }
        unsafe extern "system" fn listener(
            hwnd: HWND,
            message: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            if message == WM_CLIPBOARDUPDATE {
                let name: Vec<u16> = CAN_INCLUDE_IN_CLIPBOARD_HISTORY_FORMAT
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                let format = RegisterClipboardFormatW(PCWSTR(name.as_ptr()));
                let excluded = IsClipboardFormatAvailable(format).is_ok();
                UPDATES.with(|updates| updates.borrow_mut().push(excluded));
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        let pump = || {
            // Updates are queued to this thread; give a late second one time
            // to arrive, since its absence is what is being checked.
            let until = std::time::Instant::now() + std::time::Duration::from_secs(1);
            let mut msg = MSG::default();
            while std::time::Instant::now() < until {
                if unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                    unsafe { DispatchMessageW(&msg) };
                } else {
                    std::thread::yield_now();
                }
            }
            UPDATES.with(|updates| std::mem::take(&mut *updates.borrow_mut()))
        };

        let directory = tempfile::tempdir().unwrap();
        let image_path = directory.path().join("pixel.png");
        image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 20, 30, 255]))
            .save(&image_path)
            .unwrap();
        let image_path = image_path.to_string_lossy().into_owned();

        let backend = WindowsClipboard::new();
        unsafe {
            let instance = GetModuleHandleW(None).unwrap();
            let class = WNDCLASSW {
                lpfnWndProc: Some(listener),
                hInstance: instance.into(),
                lpszClassName: w!("CuaClipboardOnceTestListener"),
                ..Default::default()
            };
            assert_ne!(RegisterClassW(&class), 0);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class.lpszClassName,
                w!(""),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                None,
                instance,
                None,
            )
            .unwrap();
            AddClipboardFormatListener(hwnd).unwrap();
            pump();

            let png_name: Vec<u16> = "PNG".encode_utf16().chain(std::iter::once(0)).collect();
            let png_format = RegisterClipboardFormatW(PCWSTR(png_name.as_ptr()));
            let image = backend.write_image(&image_path);
            // The data itself landed, not only the update notification.
            let image_formats = (
                IsClipboardFormatAvailable(png_format).is_ok(),
                IsClipboardFormatAvailable(17).is_ok(),
            );
            let image_updates = pump();
            let file = backend.write_file_url(&image_path);
            let file_format = IsClipboardFormatAvailable(15).is_ok();
            let file_updates = pump();

            let _ = RemoveClipboardFormatListener(hwnd);
            let _ = DestroyWindow(hwnd);
            let _ = UnregisterClassW(class.lpszClassName, instance);

            // The file write first: it succeeds on every build, so its count
            // shows the transaction split even where an image write errors.
            file.unwrap();
            assert!(
                file_format,
                "write_file_url must leave CF_HDROP on the clipboard"
            );
            assert_eq!(file_updates, [true], "write_file_url clipboard updates");
            image.unwrap();
            assert_eq!(
                image_formats,
                (true, true),
                "write_image must leave PNG and CF_DIBV5 on the clipboard"
            );
            assert_eq!(image_updates, [true], "write_image clipboard updates");
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn native_clipboard_round_trips_text_when_opted_in() {
        // Overwrites the system clipboard, so it runs only where a job opts
        // in explicitly. `CI` alone is not enough: developers export it too.
        if std::env::var_os("CUA_TEST_ALLOW_CLIPBOARD").as_deref()
            != Some(std::ffi::OsStr::new("1"))
        {
            return;
        }
        let _serial = NATIVE_CLIPBOARD_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let backend = WindowsClipboard::new();
        backend
            .write_text("cua-driver clipboard test".into())
            .unwrap();
        assert_eq!(
            backend.read_text().unwrap().as_deref(),
            Some("cua-driver clipboard test")
        );
        let formats = backend.available_formats().unwrap();
        for expected in [
            EXCLUDE_CLIPBOARD_MONITOR_FORMAT,
            CAN_INCLUDE_IN_CLIPBOARD_HISTORY_FORMAT,
            CAN_UPLOAD_TO_CLOUD_CLIPBOARD_FORMAT,
        ] {
            assert!(
                formats.iter().any(|f| f == expected),
                "missing {expected} in {formats:?}"
            );
        }
    }
}
