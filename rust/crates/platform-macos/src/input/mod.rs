//! Background input synthesis for macOS.
//!
//! Two strategies:
//! 1. **AX action** (`element_index` path): `AXUIElementPerformAction` — pure
//!    RPC, works on hidden/backgrounded windows, no cursor move, no focus steal.
//! 2. **CGEvent / SkyLight** (`x, y` path): synthesize CGEvents and post them
//!    to the target pid. Prefers `SLEventPostToPid` (SkyLight SPI) over the
//!    public `CGEventPostToPid` to reach Catalyst/Chromium apps and trigger
//!    the activity-monitor tickle required for live-input detection.

pub mod ax_actions;
pub mod electron_drag;
pub(crate) mod gesture;
pub(crate) mod hid_history;
pub mod hover;
pub mod interactive;
mod key_belief;
mod key_belief_ax;
mod key_belief_observer;
pub(crate) mod key_hold;
pub mod keyboard;
pub mod mouse;
pub mod pointer_toolkit;
pub mod skylight;

pub use ax_actions::perform_ax_action;
pub use interactive::{
    GesturePhase, InteractiveDeliveryMode, InteractiveInputBatch, InteractiveInputConfig,
    InteractiveInputError, InteractiveInputEvent, InteractiveInputReceipt, InteractiveInputSession,
    KeyState, Modifier, PointerButton, PointerPhase,
};
pub use keyboard::{hotkey, press_key, type_text};
pub use mouse::click_at_xy;

/// 64-bit magic marker ("GOATCUA1") stamped onto synthetic CGEvent instances
/// in kCGEventSourceUserData (field 42) so host event taps can distinguish
/// driver-synthesized input from physical human input.
pub const GOAT_CUA_EVENT_USER_DATA: i64 = 0x474F_4154_4355_4131;

/// Stamp GOAT_CUA_EVENT_USER_DATA onto event's EVENT_SOURCE_USER_DATA field.
#[inline]
pub fn tag_synthetic_event(
    event: &core_graphics::event::CGEvent,
) -> &core_graphics::event::CGEvent {
    event.set_integer_value_field(
        core_graphics::event::EventField::EVENT_SOURCE_USER_DATA,
        GOAT_CUA_EVENT_USER_DATA,
    );
    event
}
