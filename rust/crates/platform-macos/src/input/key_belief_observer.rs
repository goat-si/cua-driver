//! Accessibility notification listeners for process-bound key beliefs.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use core_foundation::base::{CFRelease, CFTypeRef, TCFType};
use core_foundation::runloop::{
    kCFRunLoopDefaultMode, CFRunLoopAddSource, CFRunLoopGetCurrent, CFRunLoopRemoveSource,
    CFRunLoopRunInMode, CFRunLoopSourceRef,
};
use core_foundation::string::{CFString, CFStringRef};

use super::key_belief::BELIEFS;
use crate::ax::bindings::{AXUIElementCreateApplication, AXUIElementRef};
use crate::ax::enablement::{process_start_stamp, ProcessStartStamp};

type Observer = *mut c_void;
type Callback = unsafe extern "C" fn(Observer, AXUIElementRef, CFStringRef, *mut c_void);

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXObserverCreate(pid: i32, callback: Callback, observer: *mut Observer) -> i32;
    fn AXObserverAddNotification(
        observer: Observer,
        element: AXUIElementRef,
        notification: CFStringRef,
        context: *mut c_void,
    ) -> i32;
    fn AXObserverGetRunLoopSource(observer: Observer) -> CFRunLoopSourceRef;
}

struct Context {
    pid: i32,
    stamp: ProcessStartStamp,
}

unsafe extern "C" fn notify(
    _: Observer,
    _: AXUIElementRef,
    name: CFStringRef,
    context: *mut c_void,
) {
    if context.is_null() || name.is_null() {
        return;
    }
    let context = &*(context as *const Context);
    let name = CFString::wrap_under_get_rule(name).to_string();
    match name.as_str() {
        "AXUIElementDestroyed" => BELIEFS.forget_if_stamp(context.pid, context.stamp),
        "AXApplicationActivated" => BELIEFS.note_activation(context.pid, context.stamp, true),
        "AXApplicationDeactivated" => BELIEFS.note_activation(context.pid, context.stamp, false),
        _ => BELIEFS.invalidate(context.pid, context.stamp),
    }
}

static REGISTRATIONS: LazyLock<Mutex<HashMap<i32, (ProcessStartStamp, bool)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Register once per process incarnation. Failure remains fail-closed for that incarnation.
pub(super) fn ensure_observer(pid: i32, stamp: ProcessStartStamp) -> bool {
    let mut registrations = REGISTRATIONS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((prior, ok)) = registrations.get(&pid) {
        if *prior == stamp {
            return *ok;
        }
    }
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name(format!("cua-ax-key-{pid}"))
        .spawn(move || observer_thread(pid, stamp, sender));
    let ok = spawned.is_ok()
        && receiver
            .recv_timeout(Duration::from_millis(500))
            .unwrap_or(false)
        && process_start_stamp(pid) == Some(stamp);
    registrations.insert(pid, (stamp, ok));
    BELIEFS.set_observing(pid, stamp, ok);
    ok
}

fn observer_thread(pid: i32, stamp: ProcessStartStamp, sender: std::sync::mpsc::SyncSender<bool>) {
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            let _ = sender.send(false);
            return;
        }
        let mut observer: Observer = std::ptr::null_mut();
        if AXObserverCreate(pid, notify, &mut observer) != 0 || observer.is_null() {
            CFRelease(app as CFTypeRef);
            let _ = sender.send(false);
            return;
        }
        let mut context = Box::new(Context { pid, stamp });
        let context_ptr = &mut *context as *mut Context as *mut c_void;
        let notifications = [
            "AXFocusedWindowChanged",
            "AXFocusedUIElementChanged",
            "AXApplicationActivated",
            "AXApplicationDeactivated",
            "AXSheetCreated",
            "AXUIElementDestroyed",
        ];
        let ok = notifications.iter().all(|name| {
            let name = CFString::new(name);
            AXObserverAddNotification(observer, app, name.as_concrete_TypeRef(), context_ptr) == 0
        });
        if !ok {
            CFRelease(observer as CFTypeRef);
            CFRelease(app as CFTypeRef);
            let _ = sender.send(false);
            return;
        }
        let source = AXObserverGetRunLoopSource(observer);
        if source.is_null() {
            CFRelease(observer as CFTypeRef);
            CFRelease(app as CFTypeRef);
            let _ = sender.send(false);
            return;
        }
        CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
        if sender.send(true).is_ok() {
            while process_start_stamp(pid) == Some(stamp) {
                CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.5, 0);
            }
        }
        BELIEFS.forget_if_stamp(pid, stamp);
        let mut registrations = REGISTRATIONS.lock().unwrap_or_else(|e| e.into_inner());
        if registrations
            .get(&pid)
            .is_some_and(|(current, _)| *current == stamp)
        {
            registrations.remove(&pid);
        }
        drop(registrations);
        CFRunLoopRemoveSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
        CFRelease(observer as CFTypeRef);
        CFRelease(app as CFTypeRef);
        drop(context);
    }
}
