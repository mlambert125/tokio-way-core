//! Tests for the middle-click clipboard: who is offered the primary selection,
//! when, and what happens to it when the source goes away.
//!
//! The clipboard's own tests live in [`super::wl_data_device`] and
//! [`super::wl_data_offer`], and this file borrows their delivery helpers. What
//! it does not borrow is their expectations: the two selections are separate
//! state reached through separate objects, and the last test here is the one
//! that says so.

use super::wl_data_device::{deliver, drain};
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use tokio::sync::mpsc::{Receiver, channel};
use tokio_util::sync::CancellationToken;
use tokio_way_core::protocol::wire_utils::{ArgReader, ArgWriter};
use tokio_way_core::protocol::{
    ObjectType, wl_display, zwp_primary_selection_device, zwp_primary_selection_offer,
    zwp_primary_selection_source,
};
use tokio_way_core::state::CompositorState;
use tokio_way_sock::WaylandEvent;

// Ids the tests give this client's objects. Deliberately clear of the ones
// `wl_data_device`'s tests use, so a client can hold both sets at once — which
// is exactly what a real one does, and what the last test needs.
const MANAGER: u32 = 5;
const DEVICE: u32 = 6;
pub const SOURCE: u32 = 7;
const SECOND_SOURCE: u32 = 8;
const SURFACE: u32 = 10;

/// A serial the client has been given, as any input event would have.
const SERIAL: u32 = 99;

// Request opcodes, named here because the handlers keep theirs private.
const CREATE_SOURCE: u16 = 0;
const GET_DEVICE: u16 = 1;
const SET_SELECTION: u16 = 0;
const SOURCE_OFFER: u16 = 0;
const SOURCE_DESTROY: u16 = 1;

/// Give a client the primary selection manager and one device.
fn add_primary_device(state: &mut CompositorState, client_id: u32) {
    state
        .clients
        .get(client_id)
        .unwrap()
        .register_client_object(MANAGER, ObjectType::ZwpPrimarySelectionDeviceManager)
        .unwrap();
    deliver(
        state,
        client_id,
        MANAGER,
        GET_DEVICE,
        ArgWriter::new().u32(DEVICE).u32(0).build(),
    );
}

/// A client with a primary selection device, a surface, and a serial it has
/// been given, so it can quote one at `set_selection`.
pub fn add_primary_client(
    state: &mut CompositorState,
    client_id: u32,
) -> (Receiver<WaylandEvent>, CancellationToken) {
    let (tx, mut rx) = channel(64);
    let token = CancellationToken::new();
    state.clients.create(client_id, tx, token.clone());

    state
        .clients
        .get(client_id)
        .unwrap()
        .register_client_object(SURFACE, ObjectType::WlSurface)
        .unwrap();
    state.create_surface(client_id, SURFACE);
    add_primary_device(state, client_id);

    state.record_input_serial(client_id, SERIAL);
    drain(&mut rx);
    (rx, token)
}

/// Make a source offering one mime type, and put it on the primary selection.
pub fn offer_primary(state: &mut CompositorState, client_id: u32, source_id: u32, mime: &str) {
    offer_primary_quoting(state, client_id, source_id, mime, SERIAL);
}

/// The same, with the client quoting a serial of the test's choosing.
fn offer_primary_quoting(
    state: &mut CompositorState,
    client_id: u32,
    source_id: u32,
    mime: &str,
    serial: u32,
) {
    deliver(
        state,
        client_id,
        MANAGER,
        CREATE_SOURCE,
        ArgWriter::new().u32(source_id).build(),
    );
    deliver(
        state,
        client_id,
        source_id,
        SOURCE_OFFER,
        ArgWriter::new().string(mime).build(),
    );
    deliver(
        state,
        client_id,
        DEVICE,
        SET_SELECTION,
        ArgWriter::new().u32(source_id).u32(serial).build(),
    );
}

/// Client 1 owns the primary selection; client 2 has focus and has been offered
/// it. Returns both sockets and the offer id client 2 was given.
fn selection_between_two_clients(
    mime: &str,
) -> (
    CompositorState,
    Receiver<WaylandEvent>,
    Receiver<WaylandEvent>,
    u32,
) {
    let mut state = crate::tests::test_state();
    let (mut owner_rx, _) = add_primary_client(&mut state, 1);
    let (mut reader_rx, _) = add_primary_client(&mut state, 2);
    state.focused_surface = Some((2, SURFACE));
    drain(&mut reader_rx);

    offer_primary(&mut state, 1, SOURCE, mime);
    drop(drain(&mut owner_rx));

    let sent = drain(&mut reader_rx);
    let offer = offer_id_from(&sent);
    (state, owner_rx, reader_rx, offer)
}

/// The offer id the compositor gave a client, read back off its socket.
fn offer_id_from(sent: &[WaylandEvent]) -> u32 {
    let data_offer = sent
        .iter()
        .find(|m| m.object_id == DEVICE && m.op_code == zwp_primary_selection_device::DATA_OFFER)
        .expect("a data_offer event");
    ArgReader::new(&data_offer.args).u32().unwrap()
}

/// The offer id in the last `selection` event, or `None` if it was a null one.
fn selection_offer(sent: &[WaylandEvent]) -> Option<u32> {
    let selection = sent
        .iter()
        .rev()
        .find(|m| m.object_id == DEVICE && m.op_code == zwp_primary_selection_device::SELECTION)
        .expect("a selection event");
    match ArgReader::new(&selection.args).u32().unwrap() {
        0 => None,
        id => Some(id),
    }
}

fn was_sent_an_error(sent: &[WaylandEvent]) -> bool {
    sent.iter()
        .any(|m| m.object_id == wl_display::OBJECT_ID && m.op_code == wl_display::ERROR)
}

/// A pipe, as a client would make before asking to read a selection.
fn pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

/// Hand the compositor a `receive` on an offer, with the write end of a pipe.
fn receive(state: &mut CompositorState, client_id: u32, offer: u32, mime: &str) -> OwnedFd {
    let (read_end, write_end) = pipe();
    state
        .clients
        .get(client_id)
        .unwrap()
        .fd_queue
        .push_back(write_end);
    deliver(
        state,
        client_id,
        offer,
        zwp_primary_selection_offer::RECEIVE,
        ArgWriter::new().string(mime).build(),
    );
    read_end
}

#[test]
fn the_focused_client_is_offered_the_selection_in_order() {
    let mut state = crate::tests::test_state();
    let (mut owner_rx, _) = add_primary_client(&mut state, 1);
    let (mut reader_rx, _) = add_primary_client(&mut state, 2);
    state.focused_surface = Some((2, SURFACE));
    drain(&mut reader_rx);

    offer_primary(&mut state, 1, SOURCE, "text/plain");
    drop(drain(&mut owner_rx));

    let sent = drain(&mut reader_rx);
    // data_offer, then the mime types, then selection: the client has to know
    // the object exists and what is in it before it is told what it is for.
    assert_eq!(sent.len(), 3);
    assert_eq!(
        (sent[0].object_id, sent[0].op_code),
        (DEVICE, zwp_primary_selection_device::DATA_OFFER)
    );
    let offer_id = ArgReader::new(&sent[0].args).u32().unwrap();
    assert!(
        offer_id >= tokio_way_core::state::SERVER_ID_BASE,
        "the compositor names the offer, so the id is from its own half"
    );

    assert_eq!(
        (sent[1].object_id, sent[1].op_code),
        (offer_id, zwp_primary_selection_offer::OFFER)
    );
    assert_eq!(
        ArgReader::new(&sent[1].args).string().unwrap(),
        "text/plain"
    );

    assert_eq!(
        (sent[2].object_id, sent[2].op_code),
        (DEVICE, zwp_primary_selection_device::SELECTION)
    );
    assert_eq!(ArgReader::new(&sent[2].args).u32().unwrap(), offer_id);
}

#[test]
fn a_client_without_focus_is_told_nothing() {
    let mut state = crate::tests::test_state();
    let (mut owner_rx, _) = add_primary_client(&mut state, 1);
    let (mut bystander_rx, _) = add_primary_client(&mut state, 2);
    state.focused_surface = Some((1, SURFACE));
    drain(&mut owner_rx);
    drain(&mut bystander_rx);

    offer_primary(&mut state, 1, SOURCE, "text/plain");

    assert!(
        drain(&mut bystander_rx).is_empty(),
        "the primary selection follows focus, and this client has not got it"
    );
}

#[test]
fn binding_a_device_while_focused_is_told_the_selection_at_once() {
    let mut state = crate::tests::test_state();
    let (_owner_rx, _) = add_primary_client(&mut state, 1);
    offer_primary(&mut state, 1, SOURCE, "text/plain");

    // A client that comes up, is focused, and only then binds the manager —
    // which is the ordinary order, since focus is decided when its window maps.
    let (tx, mut rx) = channel(64);
    state.clients.create(2, tx, CancellationToken::new());
    state
        .clients
        .get(2)
        .unwrap()
        .register_client_object(SURFACE, ObjectType::WlSurface)
        .unwrap();
    state.create_surface(2, SURFACE);
    state.focused_surface = Some((2, SURFACE));
    drain(&mut rx);

    add_primary_device(&mut state, 2);

    let sent = drain(&mut rx);
    assert_eq!(
        selection_offer(&sent),
        Some(offer_id_from(&sent)),
        "binding while focused must not leave the client with an empty selection"
    );
}

#[test]
fn a_new_selection_cancels_the_one_it_replaces() {
    let mut state = crate::tests::test_state();
    let (mut owner_rx, _) = add_primary_client(&mut state, 1);
    offer_primary(&mut state, 1, SOURCE, "text/plain");
    drain(&mut owner_rx);

    offer_primary(&mut state, 1, SECOND_SOURCE, "text/plain");

    let sent = drain(&mut owner_rx);
    assert!(
        sent.iter()
            .any(|m| m.object_id == SOURCE && m.op_code == zwp_primary_selection_source::CANCELLED),
        "the source that held the selection is told it no longer does"
    );
    assert!(
        !sent.iter().any(|m| m.object_id == SECOND_SOURCE
            && m.op_code == zwp_primary_selection_source::CANCELLED),
        "and the one that took it over is not"
    );
    assert_eq!(state.primary_selection, Some((1, SECOND_SOURCE)));
}

#[test]
fn a_serial_the_client_was_never_given_is_refused() {
    let mut state = crate::tests::test_state();
    let (mut owner_rx, token) = add_primary_client(&mut state, 1);

    offer_primary_quoting(&mut state, 1, SOURCE, "text/plain", SERIAL + 1);

    assert_eq!(
        state.primary_selection, None,
        "a serial from no event of ours is not evidence the user selected anything"
    );
    assert!(
        !was_sent_an_error(&drain(&mut owner_rx)),
        "but it is not worth ending the connection over — the client may have \
         lost a race it had no way to see"
    );
    assert!(!token.is_cancelled());
}

#[test]
fn a_source_that_is_not_the_clients_own_is_refused_quietly() {
    let mut state = crate::tests::test_state();
    let (_owner_rx, _) = add_primary_client(&mut state, 1);
    let (mut other_rx, other_token) = add_primary_client(&mut state, 2);
    offer_primary(&mut state, 1, SOURCE, "text/plain");
    drain(&mut other_rx);

    // Client 2 names client 1's source id. Ids are per-client, so this is an
    // id of nothing at all as far as client 2 is concerned.
    deliver(
        &mut state,
        2,
        DEVICE,
        SET_SELECTION,
        ArgWriter::new().u32(SOURCE).u32(SERIAL).build(),
    );

    assert_eq!(
        state.primary_selection,
        Some((1, SOURCE)),
        "the selection it does not own is left where it was"
    );
    assert!(!was_sent_an_error(&drain(&mut other_rx)));
    assert!(
        !other_token.is_cancelled(),
        "this interface has no errors, so there is nothing to refuse it with"
    );
}

#[test]
fn a_null_source_clears_the_selection() {
    let (mut state, _owner_rx, mut reader_rx, _offer) = selection_between_two_clients("text/plain");
    drain(&mut reader_rx);

    deliver(
        &mut state,
        1,
        DEVICE,
        SET_SELECTION,
        ArgWriter::new().u32(0).u32(SERIAL).build(),
    );

    assert_eq!(state.primary_selection, None);
    assert_eq!(
        selection_offer(&drain(&mut reader_rx)),
        None,
        "the focused client is told there is nothing to paste"
    );
}

#[test]
fn receive_hands_the_pipe_to_the_source_and_the_clients_talk_directly() {
    let (mut state, mut owner_rx, _reader_rx, offer) = selection_between_two_clients("text/plain");

    let read_end = receive(&mut state, 2, offer, "text/plain");

    let mut sent = drain(&mut owner_rx);
    let asked = sent
        .iter()
        .find(|m| m.object_id == SOURCE && m.op_code == zwp_primary_selection_source::SEND)
        .expect("the source should be asked to send");
    assert_eq!(
        ArgReader::new(&asked.args).string().unwrap(),
        "text/plain",
        "the source is told which mime type was asked for"
    );
    assert_eq!(asked.fds.len(), 1, "the pipe travels with the event");

    // The compositor copies no bytes: the descriptor it passed on is the one
    // the reader is waiting on, so writing to it here reaches the reader.
    let write_end = sent
        .iter_mut()
        .find(|m| m.object_id == SOURCE && m.op_code == zwp_primary_selection_source::SEND)
        .and_then(|m| m.fds.pop())
        .expect("the relayed descriptor");
    let mut writer = std::fs::File::from(write_end);
    writer.write_all(b"selected text").unwrap();
    drop(writer);

    let mut got = String::new();
    std::fs::File::from(read_end)
        .read_to_string(&mut got)
        .unwrap();
    assert_eq!(got, "selected text");
}

#[test]
fn a_mime_type_that_was_never_offered_reads_as_empty() {
    let (mut state, mut owner_rx, _reader_rx, offer) = selection_between_two_clients("text/plain");

    let read_end = receive(&mut state, 2, offer, "image/png");

    assert!(
        !drain(&mut owner_rx)
            .iter()
            .any(|m| m.object_id == SOURCE && m.op_code == zwp_primary_selection_source::SEND),
        "the source is never asked for something it did not offer"
    );
    // The compositor's copy of the pipe went out of scope with the request, so
    // the reader gets an end of file rather than a hang.
    let mut got = String::new();
    std::fs::File::from(read_end)
        .read_to_string(&mut got)
        .unwrap();
    assert_eq!(got, "");
}

#[test]
fn an_offer_outlives_the_selection_it_named_and_reads_as_empty() {
    let (mut state, mut owner_rx, _reader_rx, offer) = selection_between_two_clients("text/plain");

    // The user selects something else. The reader still holds the old offer —
    // it is a server id, so nothing has taken it out of its object map.
    offer_primary(&mut state, 1, SECOND_SOURCE, "text/plain");
    drain(&mut owner_rx);

    let read_end = receive(&mut state, 2, offer, "text/plain");

    assert!(
        !drain(&mut owner_rx)
            .iter()
            .any(|m| m.object_id == SOURCE && m.op_code == zwp_primary_selection_source::SEND),
        "a stale offer has nothing behind it"
    );
    let mut got = String::new();
    std::fs::File::from(read_end)
        .read_to_string(&mut got)
        .unwrap();
    assert_eq!(got, "");
}

#[test]
fn destroying_the_source_leaves_the_focused_client_with_nothing() {
    let (mut state, mut owner_rx, mut reader_rx, _offer) =
        selection_between_two_clients("text/plain");
    drain(&mut reader_rx);

    deliver(&mut state, 1, SOURCE, SOURCE_DESTROY, Vec::new());

    assert_eq!(state.primary_selection, None);
    assert_eq!(selection_offer(&drain(&mut reader_rx)), None);
    assert!(
        !drain(&mut owner_rx)
            .iter()
            .any(|m| m.object_id == SOURCE && m.op_code == zwp_primary_selection_source::CANCELLED),
        "a client destroying its own source does not need telling it is gone"
    );
}

#[test]
fn the_selection_dies_with_the_client_that_owns_it() {
    let (mut state, _owner_rx, mut reader_rx, offer) = selection_between_two_clients("text/plain");
    drain(&mut reader_rx);

    state.remove_client_resources(1);

    assert_eq!(state.primary_selection, None);
    assert_eq!(
        selection_offer(&drain(&mut reader_rx)),
        None,
        "the focused client is told, rather than left holding an offer that \
         would only ever read empty"
    );
    assert!(
        state.data_offers.contains_key(&(2, offer)),
        "the reader's offer object survives: it owns the id, and a server id is \
         never announced as free"
    );
}

#[test]
fn the_two_selections_do_not_disturb_each_other() {
    use super::wl_data_device::{add_data_client, offer_selection};

    let mut state = crate::tests::test_state();
    // `add_data_client` brings the surface and the serial with it, so this
    // client ends up holding both managers, as a real toolkit does.
    let (mut owner_rx, _) = add_data_client(&mut state, 1);
    add_primary_device(&mut state, 1);
    state.focused_surface = Some((1, SURFACE));
    drain(&mut owner_rx);

    offer_selection(&mut state, 1, "text/plain", SERIAL);
    offer_primary(&mut state, 1, SOURCE, "text/html");

    assert_eq!(
        state.selection,
        Some((1, super::wl_data_device::SOURCE)),
        "setting the primary selection leaves the clipboard alone"
    );
    assert_eq!(state.primary_selection, Some((1, SOURCE)));

    // And the other way: replacing the clipboard leaves the primary selection.
    offer_selection(&mut state, 1, "text/plain", SERIAL);
    assert_eq!(state.primary_selection, Some((1, SOURCE)));
}
