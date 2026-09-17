//! Setting the selection over `ext-data-control-v1`.
//!
//! Structurally the mirror of `ext.rs`: same manager, same seat, same
//! device — but instead of watching `selection` events, this module
//! creates an `ext_data_control_source_v1`, advertises MIME types on it
//! ([`crate::write::mimes_to_offer`]), and hands it to
//! `ext_data_control_device_v1.set_selection`. Never
//! `set_primary_selection` — see `crate::write`'s module doc.
//!
//! # The source must outlive this function
//!
//! `set_selection` only has to make the compositor *accept* the new
//! selection; answering `send` events (someone actually pasting) happens
//! for as long as this remains the current clipboard owner, which can be
//! long after this function returns. So the source, the device and the
//! connection are moved into a dispatch thread that keeps running until
//! the compositor tells us we've been superseded (`cancelled`) or the
//! connection dies — dropping any of them the moment `set_selection`
//! returns would destroy the source out from under whoever tries to
//! paste a second later, and the clipboard would read as empty. This is
//! the bug CLAUDE.md calls out by name for this part of the task.
//!
//! # The *process* must outlive this function too
//!
//! Keeping the dispatch thread alive is necessary but not sufficient: a
//! detached thread dies with the process, and nothing here makes the
//! process itself wait. That was the actual bug behind "it just does
//! not paste" — `hyprforge-clipmenu` set the selection, synthesized
//! Ctrl+V, and exited in the same breath the synthetic keypress was
//! sent, tearing down this very thread before the target application's
//! `Send` request could ever arrive. `set_selection` now returns a
//! [`crate::write::SelectionGuard`] (here, `Arc<Waiter>` — see
//! `Waiter::wait`'s impl for it) so the *caller* can block, with a
//! bound, until this thread reports [`SelectionOutcome::Served`] or
//! [`SelectionOutcome::Superseded`] — see
//! `crates/hyprforge-clipmenu/src/chooser.rs::Wired::choose`.

use crate::types::{Content, Mime};
use crate::write::Offers;
use crate::wayland::pipe;
use crate::write::{SelectionOutcome, Waiter};
use std::sync::Arc;
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::{self, ExtDataControlManagerV1},
    ext_data_control_offer_v1::ExtDataControlOfferV1,
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};

struct State {
    manager: Option<ExtDataControlManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    /// What this source advertises, and what it answers each type with.
    offers: Offers,
    /// Set once the compositor tells us this source is no longer the
    /// selection (replaced by someone else) or the device is finished —
    /// the dispatch loop below checks this after every event and exits,
    /// which is what lets the thread end instead of running forever.
    done: bool,
    /// What tells whoever is blocked in [`crate::write::SelectionGuard::wait`]
    /// that this source is no longer needed.
    waiter: Arc<Waiter>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
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

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        // Only bound to satisfy `get_data_device`'s argument.
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtDataControlManagerV1,
        _: ext_data_control_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Request-only interface — no events are ever sent.
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // This writer never reads `data_offer`/`selection` — that is the
        // read side's job (`ext.rs`). `finished` means the compositor is
        // done talking to this device at all, so there is nothing left
        // to serve.
        if let ext_data_control_device_v1::Event::Finished = event {
            tracing::warn!("compositor closed the ext-data-control-v1 device while writing");
            state.done = true;
            // Nothing can be served through this device anymore; let a
            // waiting caller stop waiting now rather than at its
            // timeout.
            state.waiter.signal(SelectionOutcome::Superseded);
        }
    }

    // Not optional, however little this writer cares about `data_offer`.
    // The compositor sends one to *every* data-control device whenever
    // the selection changes — including the change this writer just
    // made — and `wayland-client` cannot deliver an event that creates a
    // child object unless it is told how to build it. Without this it
    // does not ignore the event, it panics: "Missing event_created_child
    // specialization for event opcode 0". That panic happened on the
    // dispatch thread spawned by `set_selection`, after it had already
    // returned `Ok`, so the fallback to zwlr-data-control never ran and
    // nothing was left alive to serve the selection. The clipboard went
    // empty and pasting did nothing, with no error anywhere the user
    // could see it.
    event_created_child!(State, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        source: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                let bytes = state.offers.bytes_for(&Mime::new(mime_type));
                pipe::send(fd, bytes);
                // The paste actually happened. Signalled before
                // continuing to serve (rather than only once `done`),
                // since a caller waiting on this needs to know as soon
                // as it is true, and a second `Send` for another MIME
                // type in the same paste must not override it (see
                // `Waiter`'s "first signal wins" doc).
                state.waiter.signal(SelectionOutcome::Served);
            }
            ext_data_control_source_v1::Event::Cancelled => {
                // "This data source is no longer valid. The data source
                // has been replaced by another data source." — someone
                // else now owns the clipboard; destroying it and ending
                // the thread is exactly the cleanup the protocol asks
                // for, not a failure.
                source.destroy();
                state.done = true;
                state.waiter.signal(SelectionOutcome::Superseded);
            }
            _ => {}
        }
    }
}

/// Connects, offers `content`'s MIME types on a new data source, sets it
/// as the selection, and spawns the thread that keeps serving `send`
/// events until the source is cancelled or the connection dies.
///
/// Fails fast (leaving nothing running) when the compositor does not
/// advertise this protocol at all — the caller's cue to try
/// `write_wlr::set_selection` instead, matching `ext::connect`'s own
/// fallback contract on the read side.
///
/// Returns the [`Waiter`] the caller waits on (through
/// [`crate::write::SelectionGuard::wait`]) to learn when the source it
/// just created is no longer needed — see this module's "the process
/// must outlive this function too" doc.
pub fn set_selection(content: Content) -> anyhow::Result<Arc<Waiter>> {
    set_offers(Offers::of(&content))
}

/// [`set_selection`] for exactly these types and bytes — see [`Offers`].
pub fn set_offers(offers: Offers) -> anyhow::Result<Arc<Waiter>> {
    let connection = Connection::connect_to_env()?;
    let display = connection.display();
    let mut event_queue = connection.new_event_queue::<State>();
    let qh = event_queue.handle();

    let waiter = Waiter::new();
    let mut state = State {
        manager: None,
        seat: None,
        offers,
        done: false,
        waiter: waiter.clone(),
    };

    let _registry = display.get_registry(&qh, ());
    event_queue.roundtrip(&mut state)?;

    let manager = state.manager.clone().ok_or_else(|| {
        anyhow::anyhow!("compositor does not advertise ext-data-control-manager-v1")
    })?;
    let seat = state.seat.clone().ok_or_else(|| {
        anyhow::anyhow!("compositor advertises ext-data-control-manager-v1 but no wl_seat")
    })?;

    let device = manager.get_data_device(&seat, &qh, ());
    let source = manager.create_data_source(&qh, ());
    for mime in state.offers.mimes() {
        source.offer(mime.as_str().to_string());
    }
    device.set_selection(Some(&source));

    // Everything above is only *requested* once this flushes — without
    // it the compositor never actually adopts the new selection, and a
    // caller checking "did this succeed" would get a false yes.
    connection.flush()?;

    std::thread::Builder::new()
        .name("hyprforge-clip-write-ext".to_string())
        .spawn(move || {
            loop {
                if state.done {
                    break;
                }
                if let Err(e) = event_queue.blocking_dispatch(&mut state) {
                    tracing::warn!(error = %e, "ext-data-control-v1 write connection closed; the clipboard may now be empty");
                    state.waiter.signal(SelectionOutcome::Superseded);
                    break;
                }
            }
        })?;

    Ok(waiter)
}

/// The offers this writer is handed but never reads.
///
/// `event_created_child!` above makes the device able to *build* an
/// offer; this makes the offer able to receive the `offer` events that
/// follow it. Both halves are needed, and neither does anything with
/// what arrives — reading a selection is `ext.rs`'s job. Dropping the
/// events on the floor here is the deliberate part; failing to accept
/// them at all was the bug.
impl Dispatch<ExtDataControlOfferV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtDataControlOfferV1,
        _: <ExtDataControlOfferV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
