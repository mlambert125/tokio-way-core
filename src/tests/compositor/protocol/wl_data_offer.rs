//! Tests for the receiving end: the descriptor relay that is the whole of a
//! transfer, and the action negotiation that decides what a drop means.

use super::wl_data_device::{
    DEVICE, MANAGER, SOURCE, SURFACE, add_data_client, deliver, drain, offer_selection,
};
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use tokio::sync::mpsc::Receiver;
use tokio_way_core::protocol::wire_utils::{ArgReader, ArgWriter};
use tokio_way_core::protocol::wl_data_device_manager::{
    DND_ACTION_ASK, DND_ACTION_COPY, DND_ACTION_MOVE, DND_ACTION_NONE,
};
use tokio_way_core::protocol::{wl_data_device, wl_data_source, wl_display};
use tokio_way_core::state::{CompositorState, DataOffer, OfferKind};
use tokio_way_sock::WaylandEvent;

/// The offer id the compositor gave a client, read back off its socket.
fn offer_id_from(sent: &[WaylandEvent]) -> u32 {
    let data_offer = sent
        .iter()
        .find(|m| m.object_id == DEVICE && m.op_code == wl_data_device::DATA_OFFER)
        .expect("a data_offer event");
    ArgReader::new(&data_offer.args).u32().unwrap()
}

/// A pipe, as a client would make before asking to read a selection.
fn pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

/// Client 1 owns a selection; client 2 has focus and has been offered it.
fn clipboard_between_two_clients(
    mime: &str,
) -> (
    CompositorState,
    Receiver<WaylandEvent>,
    Receiver<WaylandEvent>,
    u32,
) {
    let mut state = crate::tests::test_state();
    let (mut owner_rx, _) = add_data_client(&mut state, 1);
    let (mut reader_rx, _) = add_data_client(&mut state, 2);
    state.focused_surface = Some((2, SURFACE));
    drain(&mut reader_rx);

    offer_selection(&mut state, 1, mime, 99);
    drain(&mut owner_rx);

    let sent = drain(&mut reader_rx);
    let offer = offer_id_from(&sent);
    (state, owner_rx, reader_rx, offer)
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
        tokio_way_core::protocol::wl_data_offer::RECEIVE,
        ArgWriter::new().string(mime).build(),
    );
    read_end
}

#[test]
fn receive_hands_the_pipe_to_the_source_and_the_clients_talk_directly() {
    let (mut state, mut owner_rx, _reader_rx, offer) = clipboard_between_two_clients("text/plain");

    let read_end = receive(&mut state, 2, offer, "text/plain");

    let sent = drain(&mut owner_rx);
    let asked = sent
        .iter()
        .find(|m| m.object_id == SOURCE && m.op_code == wl_data_source::SEND)
        .expect("the source should be asked to send");
    assert_eq!(
        ArgReader::new(&asked.args).string().unwrap(),
        "text/plain",
        "the source is told which mime type was asked for"
    );
    assert_eq!(asked.fds.len(), 1, "the pipe travels with the event");

    // The compositor copies no bytes: the descriptor it passed on is the one
    // the reader is waiting on, so writing to it here reaches the reader.
    let mut relayed = sent;
    let write_end = relayed
        .iter_mut()
        .find(|m| m.op_code == wl_data_source::SEND)
        .and_then(|m| m.fds.pop())
        .expect("the relayed descriptor");
    let mut writer = std::fs::File::from(write_end);
    writer.write_all(b"hello").unwrap();
    drop(writer);

    let mut got = String::new();
    std::fs::File::from(read_end)
        .read_to_string(&mut got)
        .unwrap();
    assert_eq!(got, "hello");
}

#[test]
fn receiving_a_mime_type_that_was_never_offered_closes_the_pipe() {
    let (mut state, mut owner_rx, _reader_rx, offer) = clipboard_between_two_clients("text/plain");

    let read_end = receive(&mut state, 2, offer, "image/png");

    assert!(
        drain(&mut owner_rx).is_empty(),
        "the source is not asked for something it never offered"
    );
    // The client gets an empty paste rather than a hang, which is what actually
    // happened.
    let mut got = String::new();
    std::fs::File::from(read_end)
        .read_to_string(&mut got)
        .unwrap();
    assert!(got.is_empty());
}

#[test]
fn receiving_from_an_offer_whose_source_has_gone_closes_the_pipe() {
    let (mut state, _owner_rx, _reader_rx, offer) = clipboard_between_two_clients("text/plain");

    // The owner disconnects, taking the clipboard with it.
    state.remove_client_resources(1);
    state.clients.remove(1);
    assert!(state.selection.is_none());

    let read_end = receive(&mut state, 2, offer, "text/plain");
    let mut got = String::new();
    std::fs::File::from(read_end)
        .read_to_string(&mut got)
        .unwrap();
    assert!(got.is_empty());
}

#[test]
fn an_offer_the_compositor_has_forgotten_can_still_be_destroyed() {
    // A `wl_data_offer` id is the compositor's, so `wl_display.delete_id` is
    // never sent for it and only the client's own `destroy` retires it. If the
    // compositor tore the object down when it stopped caring, that destroy
    // would land on an unknown object and disconnect a client that did nothing
    // wrong.
    let (mut state, _owner_rx, mut reader_rx, offer) = clipboard_between_two_clients("text/plain");

    state.invalidate_offer((2, offer));
    drain(&mut reader_rx);

    deliver(&mut state, 2, offer, 2 /* destroy */, Vec::new());

    let sent = drain(&mut reader_rx);
    assert!(
        !sent
            .iter()
            .any(|m| m.object_id == wl_display::OBJECT_ID && m.op_code == wl_display::ERROR),
        "destroying a forgotten offer is not an error: {sent:?}",
    );
    assert!(!state.data_offers.contains_key(&(2, offer)));
}

/// A drag offer with both sides' masks set, ready to negotiate.
fn drag_offer(source_actions: u32, offer_actions: u32, preferred: u32) -> (CompositorState, u32) {
    let mut state = crate::tests::test_state();
    let (_owner_rx, _) = add_data_client(&mut state, 1);
    let (_reader_rx, _) = add_data_client(&mut state, 2);

    deliver(
        &mut state,
        1,
        MANAGER,
        0,
        ArgWriter::new().u32(SOURCE).build(),
    );
    deliver(
        &mut state,
        1,
        SOURCE,
        0,
        ArgWriter::new().string("text/plain").build(),
    );
    if let Some(source) = state.data_sources.get_mut(&(1, SOURCE)) {
        source.actions = source_actions;
    }

    let offer_id = state
        .clients
        .get(2)
        .unwrap()
        .allocate_id_with_version(tokio_way_core::protocol::ObjectType::WlDataOffer, 3)
        .unwrap();
    state.data_offers.insert(
        (2, offer_id),
        DataOffer {
            client_id: 2,
            source: Some((1, SOURCE)),
            kind: OfferKind::Drag,
            accepted: None,
            actions: offer_actions,
            preferred_action: preferred,
            resolved_action: 0,
        },
    );
    (state, offer_id)
}

fn settled(state: &mut CompositorState, offer_id: u32) -> u32 {
    state.resolve_offer_action((2, offer_id));
    state.data_offers[&(2, offer_id)].resolved_action
}

#[test]
fn the_preferred_action_wins_when_both_sides_offer_it() {
    let (mut state, offer) = drag_offer(
        DND_ACTION_COPY | DND_ACTION_MOVE,
        DND_ACTION_COPY | DND_ACTION_MOVE,
        DND_ACTION_MOVE,
    );
    assert_eq!(settled(&mut state, offer), DND_ACTION_MOVE);
}

#[test]
fn the_lowest_shared_action_wins_when_the_preference_is_not_on_offer() {
    // The target would rather move; the source will only allow a copy.
    let (mut state, offer) = drag_offer(
        DND_ACTION_COPY,
        DND_ACTION_COPY | DND_ACTION_MOVE,
        DND_ACTION_MOVE,
    );
    assert_eq!(settled(&mut state, offer), DND_ACTION_COPY);
}

#[test]
fn masks_that_do_not_overlap_settle_on_nothing() {
    let (mut state, offer) = drag_offer(DND_ACTION_COPY, DND_ACTION_MOVE, DND_ACTION_MOVE);
    assert_eq!(settled(&mut state, offer), DND_ACTION_NONE);
}

#[test]
fn a_source_too_old_for_actions_is_taken_to_offer_a_copy() {
    let (mut state, offer) = drag_offer(0, DND_ACTION_COPY | DND_ACTION_MOVE, DND_ACTION_MOVE);
    // Version 1: the source predates `set_actions` and can only mean a copy.
    state
        .clients
        .get(1)
        .unwrap()
        .object_versions
        .insert(SOURCE, 1);
    assert_eq!(settled(&mut state, offer), DND_ACTION_COPY);
}

#[test]
fn an_action_mask_with_undefined_bits_disconnects_the_client() {
    let mut state = crate::tests::test_state();
    let (_rx, token) = add_data_client(&mut state, 1);
    deliver(
        &mut state,
        1,
        MANAGER,
        0,
        ArgWriter::new().u32(SOURCE).build(),
    );

    deliver(
        &mut state,
        1,
        SOURCE,
        2, // set_actions
        ArgWriter::new()
            .u32(DND_ACTION_COPY | DND_ACTION_ASK | 0x8000)
            .build(),
    );
    assert!(token.is_cancelled());
}

#[test]
fn a_preferred_action_outside_the_accepted_ones_disconnects_the_client() {
    let (mut state, offer) = drag_offer(DND_ACTION_COPY, DND_ACTION_COPY, 0);
    let token = state.clients.get(2).unwrap().cancel_token.clone();

    deliver(
        &mut state,
        2,
        offer,
        4, // set_actions
        ArgWriter::new()
            .u32(DND_ACTION_COPY)
            .u32(DND_ACTION_MOVE)
            .build(),
    );
    assert!(token.is_cancelled());
}

#[test]
fn finish_before_a_drop_disconnects_the_client() {
    let (mut state, offer) = drag_offer(DND_ACTION_COPY, DND_ACTION_COPY, DND_ACTION_COPY);
    let token = state.clients.get(2).unwrap().cancel_token.clone();

    deliver(&mut state, 2, offer, 3 /* finish */, Vec::new());
    assert!(token.is_cancelled());
}

#[test]
fn finish_after_a_drop_tells_the_source_it_is_done() {
    let (mut state, offer) = drag_offer(DND_ACTION_COPY, DND_ACTION_COPY, DND_ACTION_COPY);
    let mut owner_rx = {
        // Re-open a receiver on client 1 by draining what it already has.
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        state.clients.get(1).unwrap().sender = tx;
        rx
    };
    state.mark_offer_dropped((2, offer));

    deliver(&mut state, 2, offer, 3 /* finish */, Vec::new());

    let sent = drain(&mut owner_rx);
    assert!(
        sent.iter()
            .any(|m| m.object_id == SOURCE && m.op_code == wl_data_source::DND_FINISHED),
        "the source should hear that the target is done: {sent:?}",
    );
    assert!(
        state.data_offers[&(2, offer)].source.is_none(),
        "a finished offer has nothing left to read"
    );
}
