//! Reading the current selection once, over `ext-data-control-v1`.
//!
//! The watcher in `ext.rs` stays connected and resolves every copy into
//! history [`crate::types::Content`] — text or an image. A caller that
//! wants something else, once — a file manager asking "are there files
//! on the clipboard, and which?" at the moment someone presses Paste —
//! needs neither half of that: not the staying, and not the resolving.
//!
//! Creating a data-control device makes the compositor send the current
//! selection straight away: a `data_offer`, its `offer` MIME types, then
//! `selection`. One round trip is therefore enough to learn what is on
//! the clipboard, and [`super::pipe::receive`] — the same bounded read
//! the watcher uses — fetches the one type wanted.
//!
//! `ext` only. A compositor that offers only the older
//! `zwlr-data-control-v1` gets an `Err` here, and a caller falls back to
//! whatever it can do without the system clipboard; the file manager
//! keeps its own in-process copy for exactly that case. The writers
//! still fall back to `wlr`, so what such a compositor loses is pasting
//! *in* from other applications, not copying out to them.

use crate::types::Mime;
use crate::wayland::pipe;
use std::collections::HashMap;
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::{self, ExtDataControlManagerV1},
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
};

struct State {
    manager: Option<ExtDataControlManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    offered: HashMap<ExtDataControlOfferV1, Vec<Mime>>,
    selection: Option<ExtDataControlOfferV1>,
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
        if let wl_registry::Event::Global { name, interface, version } = event {
            if interface == ExtDataControlManagerV1::interface().name {
                let bound = version.min(ExtDataControlManagerV1::interface().version);
                state.manager = Some(registry.bind::<ExtDataControlManagerV1, _, _>(name, bound, qh, ()));
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
        match event {
            ext_data_control_device_v1::Event::DataOffer { id } => {
                state.offered.insert(id, Vec::new());
            }
            ext_data_control_device_v1::Event::Selection { id } => state.selection = id,
            _ => {}
        }
    }

    // Required, not optional — see `write_ext.rs` for the panic its
    // absence caused there.
    event_created_child!(State, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.offered.entry(offer.clone()).or_default().push(Mime::new(mime_type));
        }
    }
}

/// The current selection's content in the first of `preferred` it
/// offers, or `None` when the clipboard is empty or offers none of them.
///
/// Blocks for up to one round trip plus one bounded pipe read. Call it
/// off any thread that paints — and through [`super::read_selection`],
/// which also bounds the round trip.
pub fn read(preferred: &[Mime]) -> anyhow::Result<Option<(Mime, Vec<u8>)>> {
    let connection = Connection::connect_to_env()?;
    let display = connection.display();
    let mut queue = connection.new_event_queue::<State>();
    let qh = queue.handle();
    let mut state = State { manager: None, seat: None, offered: HashMap::new(), selection: None };

    let _registry = display.get_registry(&qh, ());
    queue.roundtrip(&mut state)?;
    let manager = state
        .manager
        .clone()
        .ok_or_else(|| anyhow::anyhow!("compositor does not advertise ext-data-control-manager-v1"))?;
    let seat = state
        .seat
        .clone()
        .ok_or_else(|| anyhow::anyhow!("compositor advertises ext-data-control-manager-v1 but no wl_seat"))?;

    let device = manager.get_data_device(&seat, &qh, ());
    // The compositor sends the current selection on its own once the
    // device exists; this round trip is what waits for it.
    queue.roundtrip(&mut state)?;

    let Some(offer) = state.selection.clone() else {
        device.destroy();
        return Ok(None);
    };
    let offered = state.offered.get(&offer).cloned().unwrap_or_default();
    let Some(mime) = preferred.iter().find(|m| offered.contains(m)).cloned() else {
        device.destroy();
        return Ok(None);
    };
    let wanted = mime.as_str().to_string();
    let bytes = pipe::receive(&connection, |fd| offer.receive(wanted, fd));
    offer.destroy();
    device.destroy();
    let _ = connection.flush();
    Ok(bytes.map(|bytes| (mime, bytes)))
}
