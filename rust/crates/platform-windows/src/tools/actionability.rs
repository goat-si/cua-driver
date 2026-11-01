use crate::actionability::{limited_probe, Probe, RawLiveFacts};
use crate::uia::snapshot::RetainedElement;
use cua_driver_core::actionability::Clock;
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, LazyLock},
    time::Duration,
};
use tokio::sync::Semaphore;

static LIVE_READ_WORKERS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(2)));

pub(crate) struct TokioClock(tokio::time::Instant);
impl TokioClock {
    pub(crate) fn new() -> Self {
        Self(tokio::time::Instant::now())
    }
}
impl Clock for TokioClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(tokio::time::sleep(duration))
    }
}

pub(crate) async fn probe(element: &RetainedElement, hwnd: isize) -> Probe {
    limited_probe(LIVE_READ_WORKERS.clone(), || {
        let owned = element.clone();
        move || {
            use windows::core::Interface;
            use windows::Win32::Foundation::HWND;
            use windows::Win32::System::Com::{
                CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED,
            };
            use windows::Win32::UI::Accessibility::IUIAutomationElement;
            let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            let result = {
                // Own every interface inside this scope, before CoUninitialize.
                let retained = owned;
                if initialized.is_err() {
                    Probe::uia(RawLiveFacts::failed(format!(
                        "COM initialization failed: {initialized:?}"
                    )))
                } else {
                    let element = std::mem::ManuallyDrop::new(unsafe {
                        IUIAutomationElement::from_raw(retained.as_ptr() as *mut _)
                    });
                    match unsafe { element.CurrentNativeWindowHandle() } {
                        Err(error) => Probe::uia(RawLiveFacts::failed(format!(
                            "native window read failed: {error}"
                        ))),
                        Ok(native)
                            if crate::win32::combo::combo_for_window(native.0 as isize)
                                .is_some() =>
                        {
                            Probe::NativeCombo
                        }
                        Ok(_) => Probe::uia(unsafe {
                            crate::uia::actionability::read_live_facts(
                                &element,
                                HWND(hwnd as *mut _),
                            )
                        }),
                    }
                }
            };
            if initialized.is_ok() {
                unsafe { CoUninitialize() };
            }
            result
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    fn assert_send<T: Send>(_: &T) {}
    #[test]
    fn production_gate_future_is_send() {
        use crate::actionability::gate_then_write;
        let element =
            RetainedElement::detached(crate::uia::snapshot::SnapshotKind::Uia, (0, 0), false);
        let clock = TokioClock::new();
        let element_ref = &element;
        let future = gate_then_write(
            &clock,
            Duration::ZERO,
            move |_| probe(element_ref, 0),
            |_| async {},
        );
        assert_send(&future);
    }
}
