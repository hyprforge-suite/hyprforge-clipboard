//! Does the real compositor agree with what this crate assumes?
//!
//! The unit tests check `resolve_offer`, `select_mime` and the pipe
//! plumbing against this crate's own idea of the protocol. That is the
//! same trap the option catalogues were in before tier 2 existed: code
//! can be internally consistent and wrong about the compositor it talks
//! to. A renamed interface or a version bump is invisible here until
//! something asks the real thing.
//!
//! **Every test in this file is strictly read-only.** Nothing here sets
//! the clipboard, clears it, or claims a selection. It may *read* the
//! current selection's advertised MIME types — never receive their
//! content — because that clipboard belongs to whoever is using this
//! machine, not to a test.
//!
//! Run with `cargo test -p hyprforge-clipboard -- --ignored`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_device_v1::{
    self, ExtDataControlDeviceV1,
};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_manager_v1::{
    self, ExtDataControlManagerV1,
};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_offer_v1::{
    self, ExtDataControlOfferV1,
};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::{
    self, ZwlrDataControlManagerV1,
};

/// Same convention as `hyprforge-bluetooth`'s `live_bluez.rs` and the
/// ecosystem parse tests: libtest has no skipped state, so a check that
/// cannot run says so rather than silently reporting `ok` for zero
/// assertions.
const SKIP_MARKER: &str = "HYPRFORGE-SKIP:";

fn connection_or_skip() -> Option<Connection> {
    match Connection::connect_to_env() {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("{SKIP_MARKER} no Wayland compositor to connect to ({e})");
            None
        }
    }
}

/// Minimal registry-only state: records whether each manager global
/// was seen, and binds the ext one (if present) plus a seat so the
/// second test can go on to ask about the live selection.
struct ProbeState {
    ext_manager: Option<ExtDataControlManagerV1>,
    saw_zwlr_manager: bool,
    seat: Option<wl_seat::WlSeat>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for ProbeState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == ExtDataControlManagerV1::interface().name {
                let bound = version.min(ExtDataControlManagerV1::interface().version);
                state.ext_manager =
                    Some(registry.bind::<ExtDataControlManagerV1, _, _>(name, bound, qh, ()));
            } else if interface == ZwlrDataControlManagerV1::interface().name {
                state.saw_zwlr_manager = true;
            } else if interface == wl_seat::WlSeat::interface().name && state.seat.is_none() {
                let bound = version.min(wl_seat::WlSeat::interface().version);
                state.seat = Some(registry.bind::<wl_seat::WlSeat, _, _>(name, bound, qh, ()));
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for ProbeState {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for ProbeState {
    fn event(
        _: &mut Self,
        _: &ExtDataControlManagerV1,
        event: ext_data_control_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let _ = event;
    }
}

// Only needed so `ProbeState` can bind the zwlr manager if that is all
// a given compositor offers — never actually bound here; kept so the
// second test can name what claim it is checking even on a compositor
// that only speaks the deprecated protocol.
impl Dispatch<ZwlrDataControlManagerV1, ()> for ProbeState {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlManagerV1,
        event: zwlr_data_control_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let _ = event;
    }
}

/// The claim every other part of this crate rests on: the compositor
/// advertises *some* data-control manager at all.
#[test]
#[ignore]
fn a_data_control_manager_is_advertised() {
    let Some(connection) = connection_or_skip() else {
        return;
    };
    let display = connection.display();
    let mut event_queue = connection.new_event_queue::<ProbeState>();
    let qh = event_queue.handle();
    let mut state = ProbeState {
        ext_manager: None,
        saw_zwlr_manager: false,
        seat: None,
    };
    let _registry = display.get_registry(&qh, ());

    if let Err(e) = event_queue.roundtrip(&mut state) {
        eprintln!("{SKIP_MARKER} roundtrip with the compositor failed ({e})");
        return;
    }

    assert!(
        state.ext_manager.is_some() || state.saw_zwlr_manager,
        "this compositor advertises neither ext-data-control-v1 nor \
         zwlr-data-control-v1 — hyprforge-clipboard has nothing to watch here"
    );
    println!(
        "ext-data-control-v1: {}, zwlr-data-control-v1: {}",
        state.ext_manager.is_some(),
        state.saw_zwlr_manager
    );
}

/// State for reading the current selection's MIME types, and nothing
/// else — no `receive` request is ever made, so no content byte of
/// whatever is presently on this machine's clipboard is ever read.
struct SelectionState {
    manager: Option<ExtDataControlManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    device: Option<ExtDataControlDeviceV1>,
    pending: HashMap<ExtDataControlOfferV1, Vec<String>>,
    selection: Arc<Mutex<Option<Vec<String>>>>,
    /// Set once a `selection` event has been seen at all (even a `None`
    /// one, for an empty clipboard) — distinguishes "nothing copied
    /// right now" from "the event never arrived in time".
    seen: Arc<Mutex<bool>>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for SelectionState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == ExtDataControlManagerV1::interface().name {
                let bound = version.min(ExtDataControlManagerV1::interface().version);
                state.manager =
                    Some(registry.bind::<ExtDataControlManagerV1, _, _>(name, bound, qh, ()));
            } else if interface == wl_seat::WlSeat::interface().name && state.seat.is_none() {
                let bound = version.min(wl_seat::WlSeat::interface().version);
                state.seat = Some(registry.bind::<wl_seat::WlSeat, _, _>(name, bound, qh, ()));
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for SelectionState {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for SelectionState {
    fn event(
        _: &mut Self,
        _: &ExtDataControlManagerV1,
        event: ext_data_control_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let _ = event;
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for SelectionState {
    fn event(
        state: &mut Self,
        _proxy: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_data_control_device_v1::Event as E;
        match event {
            E::DataOffer { id } => {
                state.pending.insert(id, Vec::new());
            }
            E::Selection { id } => {
                let mimes = id.map(|offer| state.pending.remove(&offer).unwrap_or_default());
                *state.selection.lock().unwrap() = mimes;
                *state.seen.lock().unwrap() = true;
            }
            _ => {}
        }
    }

    event_created_child!(SelectionState, ExtDataControlDeviceV1, [
        0 => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for SelectionState {
    fn event(
        state: &mut Self,
        proxy: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            state
                .pending
                .entry(proxy.clone())
                .or_default()
                .push(mime_type);
        }
    }
}

/// The current selection's MIME types can be enumerated without ever
/// reading a byte of content — only something a real compositor and a
/// real source application, talking to each other, can answer.
///
/// Read-only in the way that matters: this test never issues a
/// `receive` request, so it never asks any source application to hand
/// over what the user copied.
#[test]
#[ignore]
fn the_current_selections_mime_types_can_be_enumerated_without_reading_content() {
    let Some(connection) = connection_or_skip() else {
        return;
    };
    let display = connection.display();
    let mut event_queue = connection.new_event_queue::<SelectionState>();
    let qh = event_queue.handle();
    let mut state = SelectionState {
        manager: None,
        seat: None,
        device: None,
        pending: HashMap::new(),
        selection: Arc::new(Mutex::new(None)),
        seen: Arc::new(Mutex::new(false)),
    };
    let _registry = display.get_registry(&qh, ());

    if let Err(e) = event_queue.roundtrip(&mut state) {
        eprintln!("{SKIP_MARKER} roundtrip with the compositor failed ({e})");
        return;
    }
    let (Some(manager), Some(seat)) = (state.manager.clone(), state.seat.clone()) else {
        eprintln!("{SKIP_MARKER} this compositor does not advertise ext-data-control-v1 and a seat together");
        return;
    };

    state.device = Some(manager.get_data_device(&seat, &qh, ()));
    // The first `selection` event arrives on binding the device — a
    // deadline rather than a fixed roundtrip count, since it is a
    // regular event, not a request reply.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !*state.seen.lock().unwrap() && std::time::Instant::now() < deadline {
        if let Err(e) = event_queue.dispatch_pending(&mut state) {
            eprintln!(
                "{SKIP_MARKER} dispatch failed while waiting for the initial selection ({e})"
            );
            return;
        }
        if let Err(e) = connection.flush() {
            eprintln!("{SKIP_MARKER} flush failed while waiting for the initial selection ({e})");
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
        let _ = event_queue.prepare_read().map(|guard| guard.read());
    }

    if !*state.seen.lock().unwrap() {
        eprintln!("{SKIP_MARKER} no selection event arrived within the deadline");
        return;
    }

    let selection = state.selection.lock().unwrap().clone();
    match selection {
        Some(mimes) => {
            println!("current selection offers: {mimes:?}");
        }
        None => {
            println!("clipboard is currently empty — nothing selected");
        }
    }
}

/// `read_selection`'s one round trip, against the real compositor —
/// asked for a type no application offers, so it can only answer "none"
/// and never receives a byte of the user's clipboard.
///
/// What this proves is the part a unit test cannot: that creating a
/// device really does deliver the current selection within one round
/// trip, which is the assumption the whole one-shot read rests on. If
/// the compositor delivered it later, this would still return `Ok(None)`
/// — so the test also requires the call to finish well inside its bound,
/// which a read that had to wait for a late event would not.
#[test]
#[ignore]
fn a_one_shot_read_completes_without_reading_anything() {
    if connection_or_skip().is_none() {
        return;
    }
    let started = std::time::Instant::now();
    let result = hyprforge_clipboard::read_selection(
        vec![hyprforge_clipboard::Mime::new("application/x-hyprforge-never-offered")],
        Duration::from_secs(2),
    );
    match result {
        Ok(found) => assert_eq!(found, None, "nothing offers this type"),
        Err(e) if e.to_string().contains("does not advertise ext-data-control") => {
            eprintln!("{SKIP_MARKER} compositor has no ext-data-control-v1 ({e})");
            return;
        }
        Err(e) => panic!("the one-shot read failed against the real compositor: {e}"),
    }
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "took {:?} — the selection did not arrive within one round trip",
        started.elapsed()
    );
}
