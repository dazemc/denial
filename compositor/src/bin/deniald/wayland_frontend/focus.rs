use std::borrow::Cow;

use smithay::backend::input::KeyState;
use smithay::desktop::PopupKind;
use smithay::input::keyboard::{KeyboardHandle, KeyboardTarget, KeysymHandle, ModifiersState};
use smithay::input::{Seat, SeatHandler};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Serial};
use smithay::wayland::seat::WaylandFocus;
use smithay::xwayland::X11Surface;

use super::super::RuntimeState;

/// A keyboard target must preserve the X11 identity of Xwayland windows.
///
/// Forwarding keyboard focus directly to the associated `wl_surface` is
/// enough for `wl_keyboard`, but bypasses `X11Surface::enter` and therefore
/// never performs the ICCCM `SetInputFocus`/`WM_TAKE_FOCUS` handshake.
#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum KeyboardFocusTarget {
    Wayland(WlSurface),
    X11(X11Surface),
}

impl From<WlSurface> for KeyboardFocusTarget {
    fn from(surface: WlSurface) -> Self {
        Self::Wayland(surface)
    }
}

impl From<PopupKind> for KeyboardFocusTarget {
    fn from(popup: PopupKind) -> Self {
        Self::Wayland(popup.wl_surface().clone())
    }
}

impl From<KeyboardFocusTarget> for WlSurface {
    fn from(target: KeyboardFocusTarget) -> Self {
        match target {
            KeyboardFocusTarget::Wayland(surface) => surface,
            KeyboardFocusTarget::X11(surface) => surface
                .wl_surface()
                .expect("focused X11 window has no associated wl_surface"),
        }
    }
}

impl IsAlive for KeyboardFocusTarget {
    fn alive(&self) -> bool {
        match self {
            Self::Wayland(surface) => surface.alive(),
            Self::X11(surface) => surface.alive(),
        }
    }
}

impl KeyboardFocusTarget {
    fn inner(&self) -> &dyn KeyboardTarget<RuntimeState> {
        match self {
            Self::Wayland(surface) => surface,
            Self::X11(surface) => surface,
        }
    }
}

impl KeyboardTarget<RuntimeState> for KeyboardFocusTarget {
    fn enter(
        &self,
        seat: &Seat<RuntimeState>,
        data: &mut RuntimeState,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        self.inner().enter(seat, data, keys, serial);
    }

    fn leave(&self, seat: &Seat<RuntimeState>, data: &mut RuntimeState, serial: Serial) {
        self.inner().leave(seat, data, serial);
    }

    fn key(
        &self,
        seat: &Seat<RuntimeState>,
        data: &mut RuntimeState,
        key: KeysymHandle<'_>,
        state: KeyState,
        serial: Serial,
        time: u32,
    ) {
        self.inner().key(seat, data, key, state, serial, time);
    }

    fn modifiers(
        &self,
        seat: &Seat<RuntimeState>,
        data: &mut RuntimeState,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        self.inner().modifiers(seat, data, modifiers, serial);
    }
}

impl WaylandFocus for KeyboardFocusTarget {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            Self::Wayland(surface) => Some(Cow::Borrowed(surface)),
            Self::X11(surface) => surface.wl_surface().map(Cow::Owned),
        }
    }
}

/// Clear the keyboard focus and publish the transition to seat-owned state.
///
/// Smithay reports focus replacements through `SeatHandler::focus_changed`,
/// but an explicit transition to `None` only sends `wl_keyboard.leave`.
/// Denial's text-input broker and data-device focus still need that transition.
pub(super) fn clear_keyboard_focus(
    state: &mut RuntimeState,
    keyboard: &KeyboardHandle<RuntimeState>,
    serial: Serial,
) {
    let had_focus = keyboard.current_focus().is_some();
    keyboard.set_focus(state, None, serial);
    if had_focus && keyboard.current_focus().is_none() {
        let seat = state
            .wayland
            .as_ref()
            .expect("missing Wayland frontend")
            .seat
            .clone();
        <RuntimeState as SeatHandler>::focus_changed(state, &seat, None);
    }
}

/// Request a client keyboard target without violating shell ownership.
///
/// Window activation can race the Flutter layout update which releases an
/// overlay. While the shell still captures the keyboard, retain the newest
/// client target for the handoff instead of sending it an early `enter`.
pub(super) fn request_keyboard_focus(
    state: &mut RuntimeState,
    keyboard: &KeyboardHandle<RuntimeState>,
    focus: Option<KeyboardFocusTarget>,
    serial: Serial,
) {
    #[cfg(feature = "flutter")]
    if state
        .wayland
        .as_ref()
        .is_some_and(|frontend| frontend.text_input.shell_captures_keyboard())
    {
        state
            .wayland
            .as_mut()
            .expect("missing Wayland frontend")
            .shell_keyboard_focus = focus;
        return;
    }
    keyboard.set_focus(state, focus, serial);
}

#[cfg(feature = "flutter")]
pub(in super::super) fn suspend_keyboard_focus_for_shell(state: &mut RuntimeState) {
    let keyboard = state
        .wayland
        .as_ref()
        .expect("missing Wayland frontend")
        .seat
        .get_keyboard()
        .expect("seat has no keyboard");
    let focus = keyboard.current_focus().filter(IsAlive::alive);
    state
        .wayland
        .as_mut()
        .expect("missing Wayland frontend")
        .shell_keyboard_focus = focus;

    clear_keyboard_focus(
        state,
        &keyboard,
        smithay::utils::SERIAL_COUNTER.next_serial(),
    );
    if keyboard.current_focus().is_some() {
        // Popup grabs intentionally reject ordinary focus changes. Shell
        // capture is a compositor focus transfer, so retire such a grab and
        // complete the same leave sequence.
        keyboard.unset_grab(state);
        clear_keyboard_focus(
            state,
            &keyboard,
            smithay::utils::SERIAL_COUNTER.next_serial(),
        );
    }
}

#[cfg(feature = "flutter")]
pub(in super::super) fn restore_shell_keyboard_focus(state: &mut RuntimeState) {
    let keyboard = state
        .wayland
        .as_ref()
        .expect("missing Wayland frontend")
        .seat
        .get_keyboard()
        .expect("seat has no keyboard");
    let focus = state
        .wayland
        .as_mut()
        .expect("missing Wayland frontend")
        .shell_keyboard_focus
        .take()
        .filter(IsAlive::alive);
    if keyboard.current_focus().is_none() {
        request_keyboard_focus(
            state,
            &keyboard,
            focus,
            smithay::utils::SERIAL_COUNTER.next_serial(),
        );
    }
}
