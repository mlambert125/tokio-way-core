//! Tests for the clipboard: who is offered the selection, when, and what they
//! are sent.

use tokio::sync::mpsc::{Receiver, channel};
use tokio_util::sync::CancellationToken;
use tokio_way_core::protocol::wire_utils::{ArgReader, ArgWriter};
use tokio_way_core::protocol::{ObjectType, handle_message, wl_data_device, wl_data_source};
use tokio_way_core::state::{CompositorState, DataSourceRole, OfferKind};
use tokio_way_sock::{WaylandEvent, WaylandRequest, WaylandRequestWithClientInfo};

pub const MANAGER: u32 = 2;
pub const DEVICE: u32 = 3;
pub const SOURCE: u32 = 4;
pub const SURFACE: u32 = 10;

/// A client with a data device, a surface, and a serial it has been given.
///
/// Returns the receiver its socket task would be draining and the token that
/// says whether it has been disconnected.
pub fn add_data_client(
    state: &mut CompositorState,
    client_id: u32,
) -> (Receiver<WaylandEvent>, CancellationToken) {
    let (tx, mut rx) = channel(64);
    let token = CancellationToken::new();
    state.clients.create(client_id, tx, token.clone());

    let client = state.clients.get(client_id).unwrap();
    client
        .register_with_version(MANAGER, ObjectType::WlDataDeviceManager, 3)
        .unwrap();
    client.register(SURFACE, ObjectType::WlSurface).unwrap();
    state.create_surface(client_id, SURFACE);

    deliver(
        state,
        client_id,
        MANAGER,
        1, // get_data_device
        ArgWriter::new().u32(DEVICE).u32(0).build(),
    );

    // The client has been given a serial, as it would have been by any input
    // event, so it can quote one at `set_selection`.
    state.record_input_serial(client_id, 99);
    drain(&mut rx);
    (rx, token)
}

pub fn deliver(
    state: &mut CompositorState,
    client_id: u32,
    object_id: u32,
    op_code: u16,
    args: Vec<u8>,
) {
    handle_message(
        state,
        &WaylandRequestWithClientInfo {
            client_id,
            message: WaylandRequest {
                object_id,
                op_code,
                args,
            },
        },
    );
}

pub fn drain(rx: &mut Receiver<WaylandEvent>) -> Vec<WaylandEvent> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

/// Make a source offering one mime type, and put it on the clipboard.
pub fn offer_selection(state: &mut CompositorState, client_id: u32, mime: &str, serial: u32) {
    deliver(
        state,
        client_id,
        MANAGER,
        0, // create_data_source
        ArgWriter::new().u32(SOURCE).build(),
    );
    deliver(
        state,
        client_id,
        SOURCE,
        0, // offer
        ArgWriter::new().string(mime).build(),
    );
    deliver(
        state,
        client_id,
        DEVICE,
        1, // set_selection
        ArgWriter::new().u32(SOURCE).u32(serial).build(),
    );
}

fn was_disconnected(token: &CancellationToken) -> bool {
    token.is_cancelled()
}

#[test]
fn the_focused_client_is_offered_the_selection_in_order() {
    let mut state = crate::tests::test_state();
    let (mut owner_rx, _) = add_data_client(&mut state, 1);
    let (mut reader_rx, _) = add_data_client(&mut state, 2);
    state.focused_surface = Some((2, SURFACE));
    drain(&mut reader_rx);

    offer_selection(&mut state, 1, "text/plain", 99);
    drop(drain(&mut owner_rx));

    let sent = drain(&mut reader_rx);
    // data_offer, then the mime types, then selection: the client has to know
    // the object exists and what is in it before it is told it is the
    // clipboard.
    assert_eq!(sent.len(), 3);
    assert_eq!(
        (sent[0].object_id, sent[0].op_code),
        (DEVICE, wl_data_device::DATA_OFFER)
    );
    let offer_id = ArgReader::new(&sent[0].args).u32().unwrap();
    assert!(
        offer_id >= tokio_way_core::state::SERVER_ID_BASE,
        "the compositor names the offer, so the id is from its own half"
    );

    assert_eq!((sent[1].object_id, sent[1].op_code), (offer_id, 0));
    assert_eq!(
        ArgReader::new(&sent[1].args).string().unwrap(),
        "text/plain"
    );

    assert_eq!(
        (sent[2].object_id, sent[2].op_code),
        (DEVICE, wl_data_device::SELECTION)
    );
    assert_eq!(ArgReader::new(&sent[2].args).u32().unwrap(), offer_id);
}

#[test]
fn a_client_is_offered_its_own_selection_back() {
    // Copying and pasting within one application goes through this path, so a
    // filter that skipped the source's own client would break it.
    let mut state = crate::tests::test_state();
    let (mut rx, _) = add_data_client(&mut state, 1);
    state.focused_surface = Some((1, SURFACE));
    drain(&mut rx);

    offer_selection(&mut state, 1, "text/plain", 99);

    let sent = drain(&mut rx);
    assert!(
        sent.iter()
            .any(|m| m.op_code == wl_data_device::SELECTION
                && ArgReader::new(&m.args).u32() != Some(0)),
        "the owner should still be offered a readable selection"
    );
}

#[test]
fn an_unfocused_client_is_not_offered_the_selection() {
    let mut state = crate::tests::test_state();
    let (_owner_rx, _) = add_data_client(&mut state, 1);
    let (mut idle_rx, _) = add_data_client(&mut state, 2);
    // Nothing is focused at all.
    offer_selection(&mut state, 1, "text/plain", 99);

    assert!(drain(&mut idle_rx).is_empty());
}

#[test]
fn a_client_binding_a_device_while_focused_is_offered_the_selection_at_once() {
    // A client usually gets its data device long after its first window has
    // focus. Without this it would never hear about the clipboard.
    let mut state = crate::tests::test_state();
    let (_owner_rx, _) = add_data_client(&mut state, 1);
    let (mut late_rx, _) = add_data_client(&mut state, 2);
    state.focused_surface = Some((1, SURFACE));
    offer_selection(&mut state, 1, "text/plain", 99);

    // Now client 2 gains focus and asks for a *second* device.
    state.focused_surface = Some((2, SURFACE));
    drain(&mut late_rx);
    deliver(
        &mut state,
        2,
        MANAGER,
        1,
        ArgWriter::new().u32(DEVICE + 1).u32(0).build(),
    );

    let sent = drain(&mut late_rx);
    assert!(
        sent.iter()
            .any(|m| m.object_id == DEVICE + 1 && m.op_code == wl_data_device::SELECTION),
        "the new device should be told what is on the clipboard: {sent:?}",
    );
}

#[test]
fn a_null_source_clears_the_clipboard() {
    let mut state = crate::tests::test_state();
    let (mut rx, _) = add_data_client(&mut state, 1);
    state.focused_surface = Some((1, SURFACE));
    offer_selection(&mut state, 1, "text/plain", 99);
    drain(&mut rx);

    deliver(
        &mut state,
        1,
        DEVICE,
        1,
        ArgWriter::new().u32(0).u32(99).build(),
    );

    assert!(state.selection.is_none());
    let sent = drain(&mut rx);
    let selection = sent
        .iter()
        .rev()
        .find(|m| m.op_code == wl_data_device::SELECTION)
        .expect("a null selection should be sent");
    assert_eq!(ArgReader::new(&selection.args).u32(), Some(0));
}

#[test]
fn replacing_the_selection_cancels_the_source_it_displaced() {
    let mut state = crate::tests::test_state();
    let (mut rx, _) = add_data_client(&mut state, 1);
    state.focused_surface = Some((1, SURFACE));
    offer_selection(&mut state, 1, "text/plain", 99);
    drain(&mut rx);

    // A second source replaces the first.
    deliver(
        &mut state,
        1,
        MANAGER,
        0,
        ArgWriter::new().u32(SOURCE + 1).build(),
    );
    deliver(
        &mut state,
        1,
        DEVICE,
        1,
        ArgWriter::new().u32(SOURCE + 1).u32(99).build(),
    );

    let sent = drain(&mut rx);
    assert!(
        sent.iter()
            .any(|m| m.object_id == SOURCE && m.op_code == wl_data_source::CANCELLED),
        "the displaced source should be cancelled: {sent:?}",
    );
}

#[test]
fn a_serial_we_never_sent_is_refused_without_disconnecting_the_client() {
    let mut state = crate::tests::test_state();
    let (_rx, token) = add_data_client(&mut state, 1);
    state.focused_surface = Some((1, SURFACE));

    offer_selection(&mut state, 1, "text/plain", 12345);

    assert!(state.selection.is_none(), "a stale serial sets nothing");
    assert!(
        !was_disconnected(&token),
        "a stale serial is a race the client lost, not a protocol violation"
    );
}

#[test]
fn a_serial_from_several_events_ago_is_still_honoured() {
    // A client reads a batch of events and acts on the first, so the serial it
    // quotes is not necessarily the newest one we sent.
    let mut state = crate::tests::test_state();
    let (_rx, _) = add_data_client(&mut state, 1);
    state.focused_surface = Some((1, SURFACE));
    for serial in 100..110 {
        state.record_input_serial(1, serial);
    }

    offer_selection(&mut state, 1, "text/plain", 100);
    assert_eq!(state.selection, Some((1, SOURCE)));
}

#[test]
fn a_source_may_only_be_offered_once() {
    let mut state = crate::tests::test_state();
    let (_rx, token) = add_data_client(&mut state, 1);
    state.focused_surface = Some((1, SURFACE));
    offer_selection(&mut state, 1, "text/plain", 99);
    assert_eq!(
        state.data_sources.get(&(1, SOURCE)).map(|s| s.role),
        Some(DataSourceRole::Selection)
    );

    // Offering it again is `used_source`, which is fatal.
    deliver(
        &mut state,
        1,
        DEVICE,
        1,
        ArgWriter::new().u32(SOURCE).u32(99).build(),
    );
    assert!(was_disconnected(&token));
}

#[test]
fn a_selection_offer_carries_no_action_to_settle() {
    // Actions are a drag concept. A selection offer that tried to negotiate one
    // would send `action` events for a transfer that has no target.
    let mut state = crate::tests::test_state();
    let (mut rx, _) = add_data_client(&mut state, 1);
    state.focused_surface = Some((1, SURFACE));
    offer_selection(&mut state, 1, "text/plain", 99);
    drain(&mut rx);

    let offer = *state
        .data_offers
        .iter()
        .find(|(_, o)| o.kind == OfferKind::Selection)
        .expect("a selection offer")
        .0;
    state.resolve_offer_action(offer);

    assert!(drain(&mut rx).is_empty());
}
