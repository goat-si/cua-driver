use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use cua_driver_core::guarded_dispatch::{run_guarded, GuardedResult, SendFailure};
use cua_driver_core::window_basis::BasisRefusal;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use crate::basis_events::{plan, Event, Requests, Sink};
use crate::basis_x11_errors::{reply_error, request_error};

struct X11Requests<'a> {
    conn: &'a RustConnection,
    root: Window,
    target: Window,
    screen: (i16, i16),
    local: (i16, i16),
    foreground: bool,
}

impl Requests for X11Requests<'_> {
    fn prepare(&mut self) -> Vec<&'static str> {
        if self.foreground {
            vec!["activation", "input_focus"]
        } else {
            vec![]
        }
    }

    fn submit(&mut self, event: Event) -> Result<(), SendFailure> {
        let request = if self.foreground {
            let (kind, detail) = match event {
                Event::Motion => (MOTION_NOTIFY_EVENT, 0),
                Event::KeyPress(key) => (KEY_PRESS_EVENT, key),
                Event::KeyRelease(key) => (KEY_RELEASE_EVENT, key),
                Event::ButtonPress(button) => (BUTTON_PRESS_EVENT, button),
                Event::ButtonRelease(button) => (BUTTON_RELEASE_EVENT, button),
            };
            self.conn
                .xtest_fake_input(kind, detail, 0, self.root, self.screen.0, self.screen.1, 0)
        } else {
            let (kind, button, mask, state) = match event {
                Event::ButtonPress(button) => (
                    BUTTON_PRESS_EVENT,
                    button,
                    EventMask::BUTTON_PRESS,
                    KeyButMask::default(),
                ),
                Event::ButtonRelease(button) => (
                    BUTTON_RELEASE_EVENT,
                    button,
                    EventMask::BUTTON_RELEASE,
                    match button {
                        1 => KeyButMask::BUTTON1,
                        2 => KeyButMask::BUTTON2,
                        3 => KeyButMask::BUTTON3,
                        _ => unreachable!("validated button"),
                    },
                ),
                _ => unreachable!("background plan contains buttons only"),
            };
            let event = ButtonPressEvent {
                response_type: kind,
                detail: button,
                sequence: 0,
                time: x11rb::CURRENT_TIME,
                root: self.root,
                event: self.target,
                child: x11rb::NONE,
                root_x: self.screen.0,
                root_y: self.screen.1,
                event_x: self.local.0,
                event_y: self.local.1,
                state,
                same_screen: true,
            };
            self.conn.send_event(false, self.target, mask, event)
        };
        let cookie = request.map_err(|error| SendFailure {
            emitted: request_error(&error),
            error: error.to_string(),
        })?;
        cookie.check().map_err(|error| SendFailure {
            emitted: reply_error(&error),
            error: error.to_string(),
        })
    }
}

fn activate(conn: &RustConnection, root: Window, target: Window) -> Result<()> {
    let atom = conn
        .intern_atom(false, b"_NET_ACTIVE_WINDOW")?
        .reply()?
        .atom;
    let prior = conn.get_input_focus()?.reply()?.focus;
    let event = ClientMessageEvent::new(32, target, atom, [2, x11rb::CURRENT_TIME, prior, 0, 0]);
    conn.send_event(
        false,
        root,
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        event,
    )?
    .check()?;
    conn.set_input_focus(InputFocus::PARENT, target, x11rb::CURRENT_TIME)?
        .check()?;
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        if conn.get_input_focus()?.reply()?.focus == target {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("target did not acquire input focus");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Dispatch a basis-checked X11 gesture. The guard runs after preparation and before input.
#[allow(clippy::too_many_arguments)]
pub fn dispatch(
    xid: u32,
    screen_point: (i32, i32),
    local_point: (i32, i32),
    foreground: bool,
    modifiers: &[String],
    button: u8,
    count: usize,
    guard: impl FnOnce() -> Result<(), BasisRefusal>,
) -> Result<GuardedResult> {
    if !(1..=3).contains(&button) {
        bail!("unknown button: {button}");
    }
    if count == 0 || count > 2 {
        bail!("invalid click count: {count}");
    }
    if !foreground && !modifiers.is_empty() {
        bail!("modifiers require foreground");
    }
    let keysyms = modifiers
        .iter()
        .map(|modifier| match modifier.to_ascii_lowercase().as_str() {
            "shift" | "ctrl" | "control" | "alt" | "super" | "meta" | "win" | "cmd" => {
                crate::input::key_name_to_keysym(modifier)
            }
            _ => bail!("unknown modifier: {modifier}"),
        })
        .collect::<Result<Vec<_>>>()?;
    let screen = (
        i16::try_from(screen_point.0)?,
        i16::try_from(screen_point.1)?,
    );
    let local = (i16::try_from(local_point.0)?, i16::try_from(local_point.1)?);
    let (conn, screen_num) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen_num].root;
    let mut keys = Vec::new();
    if !keysyms.is_empty() {
        let setup = conn.setup();
        let first = setup.min_keycode;
        let count = setup.max_keycode - first + 1;
        let mapping = conn.get_keyboard_mapping(first, count)?.reply()?;
        let per = usize::from(mapping.keysyms_per_keycode);
        if per == 0 {
            bail!("empty keyboard mapping");
        }
        for keysym in keysyms {
            let index = mapping
                .keysyms
                .chunks(per)
                .position(|syms| syms.contains(&keysym))
                .context("modifier has no keycode")?;
            keys.push(first + u8::try_from(index)?);
        }
    }
    if foreground {
        activate(&conn, root, xid)?;
    }
    let events = plan(foreground, &keys, button, count);
    let planned = events.len();
    let mut sink = Sink::new(
        X11Requests {
            conn: &conn,
            root,
            target: xid,
            screen,
            local,
            foreground,
        },
        events,
    );
    Ok(run_guarded(&mut sink, planned, guard))
}
