use std::sync::Mutex;

use windows::core::{Interface, HRESULT};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Accessibility::{CUIAutomation8, IUIAutomation, IUIAutomation2};
use windows::Win32::UI::WindowsAndMessaging::IsWindow;

use crate::uia_root::{probe_with, warm_up_with, RootVerdict};

const CONNECTION_TIMEOUT_MS: u32 = 2_000;
const TRANSACTION_TIMEOUT_MS: u32 = 60_000;
const E_FAIL: i32 = 0x80004005u32 as i32;
static FIRST_TOUCH: Mutex<bool> = Mutex::new(false);

pub(crate) fn warm_up(client: &IUIAutomation) -> windows::core::Result<()> {
    match warm_up_with(&FIRST_TOUCH, || unsafe {
        client.GetRootElement().map(|_| ()).map_err(|e| e.code().0)
    }) {
        Err(E_FAIL) => Err(windows::core::Error::from_hresult(HRESULT(E_FAIL))),
        _ => Ok(()),
    }
}

pub(crate) fn new_uia_client() -> windows::core::Result<IUIAutomation> {
    unsafe {
        let client: IUIAutomation2 = CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)?;
        client.SetConnectionTimeout(CONNECTION_TIMEOUT_MS)?;
        client.SetTransactionTimeout(TRANSACTION_TIMEOUT_MS)?;
        let client: IUIAutomation = client.cast()?;
        warm_up(&client)?;
        Ok(client)
    }
}

pub(crate) fn probe_root(hwnd: HWND) -> RootVerdict {
    probe_with(unsafe { IsWindow(hwnd).as_bool() }, || {
        crate::input::inject::window_thread_responds(hwnd, 1_000)
    })
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
