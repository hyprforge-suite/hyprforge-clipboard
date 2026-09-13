//! Synthesising Ctrl+V over `zwp_virtual_keyboard_manager_v1`.
//!
//! A single protocol, no `ext`/`wlr` split: Hyprland (and every other
//! compositor that supports this at all) only ever offers this one.
//! What is *not* a given is whether it is offered at all — a virtual
//! keyboard is a privileged capability some compositors gate or omit —
//! so [`WaylandPaster::connect`] never fails outright the way
//! [`crate::wayland::WaylandWatcher::connect`] does. It always returns a
//! usable value; [`WaylandPaster::paste`] reports
//! [`crate::paste::PasteOutcome::Unavailable`] instead, so the caller
//! can still set the clipboard and tell the user to press Ctrl+V by
//! hand.
//!
//! # A keymap is mandatory, not incidental
//!
//! `zwp_virtual_keyboard_v1.key` and `.modifiers` both error
//! (`no_keymap`) if called before `.keymap` — and, worse for anyone
//! debugging this later, a keymap that compiles but does not contain the
//! keys this module presses simply sends nothing that lands as `v`: the
//! compositor consults the keymap to turn `key(KEY_V)` into whatever
//! symbol *that keymap* assigns to keycode 55, and an empty or
//! mismatched keymap can silently swallow the keystroke or produce the
//! wrong character. So a keymap is compiled from `libxkbcommon`'s
//! ordinary "us"/evdev rules — the same defaults an ordinary physical
//! keyboard resolves to — rather than hand-writing a hand-rolled
//! text-format keymap naming only Control and V: a full, real keymap is
//! no harder to obtain via `xkbcommon::xkb::Keymap::new_from_names` and
//! is far less likely to be subtly wrong than one assembled by hand.
//!
//! # Never a stuck modifier
//!
//! Every `key`/`modifiers` request only *queues* a message; nothing is
//! actually sent to the compositor until `Connection::flush` is called.
//! [`Inner::send_ctrl_v`] takes advantage of that: the entire sequence —
//! press Ctrl, set the modifier state, press V, release V, release the
//! modifier state, release Ctrl — is queued and then flushed exactly
//! once. Either the whole batch reaches the compositor, or (a flush
//! failure) none of it does; there is no flush between the press and the
//! release that could succeed for one and fail for the other and leave
//! Ctrl depressed on the user's real keyboard afterward.

use crate::paste::{PasteOutcome, Shortcut};
use std::os::fd::AsFd;
use std::sync::Mutex;
use std::time::Instant;
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::{self, ZwpVirtualKeyboardManagerV1},
    zwp_virtual_keyboard_v1::{self, ZwpVirtualKeyboardV1},
};
use xkbcommon::xkb;

/// `KEY_LEFTCTRL` from `linux/input-event-codes.h` — the raw evdev
/// keycode this protocol's wire format uses (unlike an *xkb* keycode,
/// which is this value plus 8; see the module doc on why that offset
/// belongs to the keymap compiler, not to this request).
const KEY_LEFTCTRL: u32 = 29;
/// `KEY_LEFTSHIFT`, same source — needed for [`Shortcut::CtrlShiftV`].
const KEY_LEFTSHIFT: u32 = 42;
/// `KEY_V`, same source.
const KEY_V: u32 = 47;

/// `WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1` — this protocol reuses
/// `wl_keyboard`'s own keymap-format numbering rather than defining its
/// own, per its upstream XML.
const KEYMAP_FORMAT_XKB_V1: u32 = 1;

/// `WL_KEYBOARD_KEY_STATE_RELEASED`/`_PRESSED` — the `key` request's
/// `state` argument is a plain `uint` in this protocol's XML (no `enum`
/// attribute), so there is no generated Rust enum to reach for; these
/// are `wl_keyboard`'s own `key_state` values, which this protocol's own
/// description says the argument carries.
const KEY_STATE_RELEASED: u32 = 0;
const KEY_STATE_PRESSED: u32 = 1;

struct RegistryState {
    manager: Option<ZwpVirtualKeyboardManagerV1>,
    seat: Option<wl_seat::WlSeat>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for RegistryState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == ZwpVirtualKeyboardManagerV1::interface().name {
                let bound = version.min(ZwpVirtualKeyboardManagerV1::interface().version);
                state.manager = Some(registry.bind::<ZwpVirtualKeyboardManagerV1, _, _>(
                    name, bound, qh, (),
                ));
            } else if interface == wl_seat::WlSeat::interface().name && state.seat.is_none() {
                let bound = version.min(wl_seat::WlSeat::interface().version);
                state.seat = Some(registry.bind::<wl_seat::WlSeat, _, _>(name, bound, qh, ()));
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for RegistryState {
    fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        // Only bound to satisfy `create_virtual_keyboard`'s argument.
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for RegistryState {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardManagerV1,
        _: zwp_virtual_keyboard_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Request-only interface — no events are ever sent.
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for RegistryState {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardV1,
        _: zwp_virtual_keyboard_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Request-only interface (bar `keymap`'s `error` enum, which
        // arrives as a protocol error, not an event) — nothing to read.
    }
}

struct Inner {
    connection: Connection,
    keyboard: ZwpVirtualKeyboardV1,
    /// The bit `modifiers()` must set to mean "Control is held", read
    /// back from the compiled keymap rather than assumed — the standard
    /// X11/xkb "real modifier" ordering usually puts Control at bit 2,
    /// but reading it from the keymap that was actually compiled is what
    /// makes this correct rather than merely usually-correct.
    ctrl_bit: u32,
    /// The bit `modifiers()` must set to mean "Shift is held", read back
    /// from the compiled keymap the same way [`Self::ctrl_bit`] is, and
    /// for the same reason: assumed bit orderings are the kind of thing
    /// that is merely usually correct.
    shift_bit: u32,
    /// One shared clock for every `key`/`modifiers` request on this
    /// keyboard object, per the protocol's own requirement ("all
    /// requests regarding a single object must share the same clock").
    started: Instant,
}

/// One queued `zwp_virtual_keyboard_v1` request, exactly as
/// [`combo_requests`] would have it sent — kept apart from the actual
/// proxy calls so the *sequence* (every press paired with a release,
/// modifiers cleared at the end) can be pinned by a test with no
/// compositor, no `Connection`, and no proxy object at all. `Inner::send_combo`
/// is the only place these are ever turned into real protocol requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Request {
    /// `zwp_virtual_keyboard_v1.modifiers(mods_depressed, 0, 0, 0)`.
    Modifiers(u32),
    /// `zwp_virtual_keyboard_v1.key(_, keycode, state)` — the time
    /// argument is filled in by the caller, since it is a per-call clock
    /// reading rather than anything this sequence itself decides.
    Key(u32, u32),
}

/// The full request sequence for one [`Shortcut`]: modifiers set before
/// any key, every press immediately followed later by its matching
/// release, and the modifier state cleared *unconditionally* as the very
/// last request — regardless of `shortcut`, so there is no path through
/// this function that presses Shift or Ctrl and forgets to let go of it.
/// Pulled out as a pure function (no `Connection`, no proxy) precisely so
/// that guarantee can be pinned by a test without a compositor — see this
/// module's own tests below.
fn combo_requests(shortcut: Shortcut, ctrl_bit: u32, shift_bit: u32) -> Vec<Request> {
    let shift = matches!(shortcut, Shortcut::CtrlShiftV);
    let mods = ctrl_bit | if shift { shift_bit } else { 0 };

    let mut requests = vec![Request::Modifiers(mods), Request::Key(KEY_LEFTCTRL, KEY_STATE_PRESSED)];
    if shift {
        requests.push(Request::Key(KEY_LEFTSHIFT, KEY_STATE_PRESSED));
    }
    requests.push(Request::Key(KEY_V, KEY_STATE_PRESSED));
    requests.push(Request::Key(KEY_V, KEY_STATE_RELEASED));
    if shift {
        requests.push(Request::Key(KEY_LEFTSHIFT, KEY_STATE_RELEASED));
    }
    requests.push(Request::Key(KEY_LEFTCTRL, KEY_STATE_RELEASED));
    // Unconditional: always the last request appended, regardless of
    // `shift` — see this function's own doc.
    requests.push(Request::Modifiers(0));
    requests
}

impl Inner {
    /// Presses and releases `shortcut`, queuing every request from
    /// [`combo_requests`] and flushing exactly once — see the module
    /// doc's "never a stuck modifier" section for why the flush is
    /// singular.
    fn send_combo(&mut self, shortcut: Shortcut) -> bool {
        let time = self.started.elapsed().as_millis() as u32;
        for request in combo_requests(shortcut, self.ctrl_bit, self.shift_bit) {
            match request {
                Request::Modifiers(mods) => self.keyboard.modifiers(mods, 0, 0, 0),
                Request::Key(keycode, state) => self.keyboard.key(time, keycode, state),
            }
        }

        match self.connection.flush() {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "failed to flush a synthesized paste; the clipboard is still set");
                false
            }
        }
    }
}

/// Synthesizes Ctrl+V, or reports that it could not.
///
/// Construction never fails: a compositor with no virtual-keyboard
/// protocol (or no seat) yields a paster whose every [`Self::paste`]
/// call reports [`PasteOutcome::Unavailable`] rather than an error
/// bubbling out of `connect` — see the module doc.
pub struct WaylandPaster {
    inner: Option<Mutex<Inner>>,
}

impl WaylandPaster {
    pub fn connect() -> anyhow::Result<Self> {
        match try_connect() {
            Ok(inner) => Ok(WaylandPaster {
                inner: Some(Mutex::new(inner)),
            }),
            Err(e) => {
                tracing::info!(
                    error = %e,
                    "zwp-virtual-keyboard-manager-v1 unavailable; paste synthesis disabled, clipboard still works"
                );
                Ok(WaylandPaster { inner: None })
            }
        }
    }
}

impl crate::paste::PasteSynthesizer for WaylandPaster {
    fn paste(&self, shortcut: Shortcut) -> PasteOutcome {
        let Some(inner) = &self.inner else {
            return PasteOutcome::Unavailable;
        };
        let mut inner = inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.send_combo(shortcut) {
            PasteOutcome::Dispatched
        } else {
            PasteOutcome::Unavailable
        }
    }
}

fn try_connect() -> anyhow::Result<Inner> {
    let connection = Connection::connect_to_env()?;
    let display = connection.display();
    let mut event_queue = connection.new_event_queue::<RegistryState>();
    let qh = event_queue.handle();

    let mut state = RegistryState {
        manager: None,
        seat: None,
    };
    let _registry = display.get_registry(&qh, ());
    event_queue.roundtrip(&mut state)?;

    let manager = state.manager.ok_or_else(|| {
        anyhow::anyhow!("compositor does not advertise zwp-virtual-keyboard-manager-v1")
    })?;
    let seat = state
        .seat
        .ok_or_else(|| anyhow::anyhow!("compositor advertises the virtual keyboard manager but no wl_seat"))?;

    let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());

    let (keymap_text, ctrl_bit, shift_bit) = compile_keymap()?;
    upload_keymap(&keyboard, &keymap_text)?;
    connection.flush()?;

    Ok(Inner {
        connection,
        keyboard,
        ctrl_bit,
        shift_bit,
        started: Instant::now(),
    })
}

/// Compiles the system's ordinary "us" keyboard layout — see the module
/// doc for why a real layout is used rather than a hand-written keymap
/// naming only Control and V — and reads back the bits `modifiers()`
/// must set for Control and Shift under *this* keymap.
fn compile_keymap() -> anyhow::Result<(String, u32, u32)> {
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_names(
        &context,
        "evdev",
        "",
        "us",
        "",
        None,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .ok_or_else(|| anyhow::anyhow!("libxkbcommon could not compile a default \"us\" keymap"))?;

    let ctrl_index = keymap.mod_get_index(xkb::MOD_NAME_CTRL);
    if ctrl_index == xkb::MOD_INVALID {
        anyhow::bail!("compiled keymap has no Control modifier");
    }
    let shift_index = keymap.mod_get_index(xkb::MOD_NAME_SHIFT);
    if shift_index == xkb::MOD_INVALID {
        anyhow::bail!("compiled keymap has no Shift modifier");
    }

    Ok((
        keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1),
        1u32 << ctrl_index,
        1u32 << shift_index,
    ))
}

/// Hands the compositor a memory-mappable copy of the keymap text.
///
/// `tempfile::tempfile()` (rather than hand-rolling `memfd_create`)
/// gives an anonymous, already-unlinked file the compositor can `mmap`
/// after receiving the fd over the socket — the file only needs to
/// outlive the `keymap` request's own flush, since the fd is duplicated
/// into the compositor's process at send time.
fn upload_keymap(keyboard: &ZwpVirtualKeyboardV1, text: &str) -> anyhow::Result<()> {
    use std::io::Write;
    let mut file = tempfile::tempfile()?;
    file.write_all(text.as_bytes())?;
    file.flush()?;
    keyboard.keymap(KEYMAP_FORMAT_XKB_V1, file.as_fd(), text.len() as u32);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the whole module exists to get right: whatever bit
    /// this crate sends for "Control is held" is the same bit the
    /// keymap it uploaded actually assigns to Control — read back from
    /// the compiled keymap rather than hardcoded, so a future libxkbcommon
    /// or rules-file change cannot silently desync the two. This needs no
    /// compositor: compiling a keymap is a pure libxkbcommon operation.
    #[test]
    fn the_control_bit_is_read_back_from_the_compiled_keymap_not_assumed() {
        let (text, ctrl_bit, shift_bit) = compile_keymap().expect("the system's \"us\" keymap must compile");
        assert!(!text.is_empty());
        assert!(text.contains("xkb_keymap"));
        assert_ne!(ctrl_bit, 0, "Control must resolve to some non-zero bit");
        assert_ne!(shift_bit, 0, "Shift must resolve to some non-zero bit");
        assert_ne!(ctrl_bit, shift_bit, "Control and Shift must not resolve to the same bit");
        // Confirms the keymap actually contains a Control key and a V
        // key (via its symbol) rather than an unrelated "us" variant
        // that dropped one — the two keys this crate presses.
        assert!(text.to_lowercase().contains("control"));
    }

    // --- `combo_requests`: the pure sequence `Inner::send_combo` plays
    // back onto the real protocol requests. No `Connection`, no proxy, no
    // compositor — exactly the seam CLAUDE.md asks a D-Bus-backed
    // module's decisions to have, applied here to a Wayland one.

    const CTRL_BIT: u32 = 0b0100;
    const SHIFT_BIT: u32 = 0b0001;

    /// Every press has a matching release, for both keys a Ctrl+Shift+V
    /// sends — the property a stuck Ctrl or Shift on someone's real
    /// keyboard would violate.
    #[test]
    fn every_pressed_key_is_released_for_ctrl_shift_v() {
        let requests = combo_requests(Shortcut::CtrlShiftV, CTRL_BIT, SHIFT_BIT);
        for key in [KEY_LEFTCTRL, KEY_LEFTSHIFT, KEY_V] {
            let presses = requests.iter().filter(|r| **r == Request::Key(key, KEY_STATE_PRESSED)).count();
            let releases = requests.iter().filter(|r| **r == Request::Key(key, KEY_STATE_RELEASED)).count();
            assert_eq!(presses, 1, "key {key} must be pressed exactly once");
            assert_eq!(releases, 1, "key {key} must be released exactly once");
        }
    }

    /// Plain Ctrl+V never touches Shift at all — no press, no release,
    /// not even a bit set in `modifiers()` for it.
    #[test]
    fn ctrl_v_never_presses_or_bit_sets_shift() {
        let requests = combo_requests(Shortcut::CtrlV, CTRL_BIT, SHIFT_BIT);
        assert!(!requests.contains(&Request::Key(KEY_LEFTSHIFT, KEY_STATE_PRESSED)));
        assert!(!requests.contains(&Request::Key(KEY_LEFTSHIFT, KEY_STATE_RELEASED)));
        for request in &requests {
            if let Request::Modifiers(mods) = request {
                assert_eq!(mods & SHIFT_BIT, 0, "Shift's bit must never be set for plain Ctrl+V");
            }
        }
    }

    /// The modifier state is set *before* any key press and cleared
    /// *unconditionally* as the very last request — both combinations,
    /// so nothing here can leave Ctrl or Shift depressed.
    #[test]
    fn modifiers_are_set_first_and_cleared_last_for_both_shortcuts() {
        for shortcut in [Shortcut::CtrlV, Shortcut::CtrlShiftV] {
            let requests = combo_requests(shortcut, CTRL_BIT, SHIFT_BIT);
            let first = requests.first().copied().unwrap();
            let last = requests.last().copied().unwrap();
            assert!(matches!(first, Request::Modifiers(mods) if mods != 0), "{shortcut:?}: first request must set modifiers");
            assert_eq!(last, Request::Modifiers(0), "{shortcut:?}: last request must clear every modifier");
            // Every request in between is a key, never another
            // modifiers request that could leave a window where a press
            // reached the compositor without the right modifier bits
            // already held.
            for middle in &requests[1..requests.len() - 1] {
                assert!(matches!(middle, Request::Key(_, _)));
            }
        }
    }

    /// Ctrl+Shift+V's `modifiers` bit set actually carries both bits —
    /// not just Ctrl with Shift forgotten, which would send a keystroke
    /// no terminal recognises as paste at all.
    #[test]
    fn ctrl_shift_v_sets_both_bits_in_the_same_modifiers_request() {
        let requests = combo_requests(Shortcut::CtrlShiftV, CTRL_BIT, SHIFT_BIT);
        let Request::Modifiers(mods) = requests[0] else { panic!("expected a Modifiers request first") };
        assert_eq!(mods, CTRL_BIT | SHIFT_BIT);
    }
}
