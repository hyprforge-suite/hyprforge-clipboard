//! Setting the selection over `zwlr-data-control-v1` — the fallback used
//! only when `write_ext` reports the standardised protocol is
//! unavailable. Structurally identical to `write_ext.rs`, for the same
//! reason `wlr.rs` mirrors `ext.rs` on the read side: same requests, same
//! events, a different generated Rust type for each. See `write_ext.rs`
//! for why the source must outlive this function's return, and for what
//! `Waiter` is doing here.

use crate::types::{Content, Mime};
use crate::wayland::pipe;
use crate::write::{bytes_for, mimes_to_offer, SelectionOutcome, Waiter};
use std::sync::Arc;
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::{self, ZwlrDataControlManagerV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

struct State {
    manager: Option<ZwlrDataControlManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    content: Content,
    done: bool,
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
            if interface == ZwlrDataControlManagerV1::interface().name {
                let bound = version.min(ZwlrDataControlManagerV1::interface().version);
                state.manager =
                    Some(registry.bind::<ZwlrDataControlManagerV1, _, _>(name, bound, qh, ()));
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

impl Dispatch<ZwlrDataControlManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlManagerV1,
        _: zwlr_data_control_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Request-only interface — no events are ever sent.
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_device_v1::Event::Finished = event {
            tracing::warn!("compositor closed the zwlr-data-control-v1 device while writing");
            state.done = true;
            // The device is gone; nothing will ever serve this source
            // now. Signalling `Superseded` (rather than leaving the
            // guard to sit out the rest of its timeout) is the same
            // "someone else owns the clipboard now" shape — the source
            // is definitely not going to be pasted from through this
            // connection either way.
            state.waiter.signal(SelectionOutcome::Superseded);
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        source: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                let bytes = bytes_for(&state.content, &Mime::new(mime_type)).unwrap_or_default();
                pipe::send(fd, bytes);
                // Someone actually read the clipboard — signal this
                // before continuing to serve, since a second `Send` for
                // another MIME type in the same paste must not
                // overwrite this with a later outcome (see `Waiter`'s
                // "first signal wins" doc).
                state.waiter.signal(SelectionOutcome::Served);
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                source.destroy();
                state.done = true;
                state.waiter.signal(SelectionOutcome::Superseded);
            }
            _ => {}
        }
    }
}

/// See `write_ext::set_selection` — identical shape, the older protocol.
pub fn set_selection(content: Content) -> anyhow::Result<Arc<Waiter>> {
    let connection = Connection::connect_to_env()?;
    let display = connection.display();
    let mut event_queue = connection.new_event_queue::<State>();
    let qh = event_queue.handle();

    let waiter = Waiter::new();
    let mut state = State {
        manager: None,
        seat: None,
        content,
        done: false,
        waiter: waiter.clone(),
    };

    let _registry = display.get_registry(&qh, ());
    event_queue.roundtrip(&mut state)?;

    let manager = state.manager.clone().ok_or_else(|| {
        anyhow::anyhow!("compositor does not advertise zwlr-data-control-manager-v1")
    })?;
    let seat = state.seat.clone().ok_or_else(|| {
        anyhow::anyhow!("compositor advertises zwlr-data-control-manager-v1 but no wl_seat")
    })?;

    let device = manager.get_data_device(&seat, &qh, ());
    let source = manager.create_data_source(&qh, ());
    for mime in mimes_to_offer(&state.content) {
        source.offer(mime.as_str().to_string());
    }
    device.set_selection(Some(&source));
    connection.flush()?;

    std::thread::Builder::new()
        .name("hyprforge-clip-write-wlr".to_string())
        .spawn(move || {
            loop {
                if state.done {
                    break;
                }
                if let Err(e) = event_queue.blocking_dispatch(&mut state) {
                    tracing::warn!(error = %e, "zwlr-data-control-v1 write connection closed; the clipboard may now be empty");
                    // The connection is gone; signal so a caller
                    // blocked in `Waiter::wait` does not sit out the
                    // rest of its timeout for an answer that can no
                    // longer come.
                    state.waiter.signal(SelectionOutcome::Superseded);
                    break;
                }
            }
        })?;

    Ok(waiter)
}
