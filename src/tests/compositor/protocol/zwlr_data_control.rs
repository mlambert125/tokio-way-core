//! Tests for `wlr-data-control`: what a clipboard manager is told, what it may
//! do, and that the three data interfaces reach each other.
//!
//! The cross-interface tests are the ones that matter most here. A manager that
//! could only exchange content with other managers would be useless, so a
//! selection set through this interface has to arrive at an ordinary
//! `wl_data_device` as an ordinary offer, and the other way round — and the
//! source behind an offer is told to send through whichever interface *it*
//! speaks, not whichever one the reader does.

use super::wl_data_device::{add_data_client, deliver, drain, offer_selection};
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use tokio::sync::mpsc::{Receiver, channel};
use tokio_util::sync::CancellationToken;
use tokio_way_core::protocol::wire_utils::{ArgReader, ArgWriter};
use tokio_way_core::protocol::{
    ObjectType, wl_data_device, wl_data_offer, wl_data_source, wl_display,
    zwlr_data_control_device, zwlr_data_control_source,
};
use tokio_way_core::state::CompositorState;
use tokio_way_sock::WaylandEvent;

// The manager's own object ids, clear of the ones the `wl_data_device` tests
// use so that one client can hold both.
const MANAGER: u32 = 30;
const DEVICE: u32 = 31;
const SOURCE: u32 = 32;
const SECOND_SOURCE: u32 = 33;

// Request opcodes.
const CREATE_DATA_SOURCE: u16 = 0;
const GET_DATA_DEVICE: u16 = 1;
const SET_SELECTION: u16 = 0;
const SET_PRIMARY_SELECTION: u16 = 2;
const SOURCE_OFFER: u16 = 0;

/// A clipboard manager: a client with a data-control device and nothing else —
/// no surface, and never focused, which is the state a real one lives in.
fn add_manager(
    state: &mut CompositorState,
    client_id: u32,
    version: u32,
) -> (Receiver<WaylandEvent>, CancellationToken) {
    let (tx, rx) = channel(64);
    let token = CancellationToken::new();
    state.clients.create(client_id, tx, token.clone());
    state
        .clients
        .get(client_id)
        .unwrap()
        .register_client_object_with_version(MANAGER, ObjectType::ZwlrDataControlManager, version)
        .unwrap();
    deliver(
        state,
        client_id,
        MANAGER,
        GET_DATA_DEVICE,
        ArgWriter::new().u32(DEVICE).u32(0).build(),
    );
    (rx, token)
}

/// Make a data-control source offering one mime type.
fn make_source(state: &mut CompositorState, client_id: u32, source_id: u32, mime: &str) {
    deliver(
        state,
        client_id,
        MANAGER,
        CREATE_DATA_SOURCE,
        ArgWriter::new().u32(source_id).build(),
    );
    deliver(
        state,
        client_id,
        source_id,
        SOURCE_OFFER,
        ArgWriter::new().string(mime).build(),
    );
}

/// The offer id named by the last `selection` or `primary_selection` event, or
/// `None` if that event carried a null one.
fn selection_offer(sent: &[WaylandEvent], event: u16) -> Option<u32> {
    let selection = sent
        .iter()
        .rev()
        .find(|m| m.object_id == DEVICE && m.op_code == event)
        .expect("a selection event");
    match ArgReader::new(&selection.args).u32().unwrap() {
        0 => None,
        id => Some(id),
    }
}

/// The mime types the manager was told an offer contains.
fn mime_types_of(sent: &[WaylandEvent], offer_id: u32) -> Vec<String> {
    sent.iter()
        .filter(|m| m.object_id == offer_id && m.op_code == wl_data_offer::OFFER)
        .map(|m| ArgReader::new(&m.args).string().unwrap())
        .collect()
}

fn was_sent_an_error(sent: &[WaylandEvent]) -> bool {
    sent.iter()
        .any(|m| m.object_id == wl_display::OBJECT_ID && m.op_code == wl_display::ERROR)
}

fn pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

/// Ask to read an offer, handing over the write end of a pipe. Returns the end
/// the answer will arrive on.
fn receive(
    state: &mut CompositorState,
    reader: u32,
    offer: u32,
    receive_op: u16,
    mime: &str,
) -> OwnedFd {
    let (read_end, write_end) = pipe();
    state
        .clients
        .get(reader)
        .unwrap()
        .fd_queue
        .push_back(write_end);
    deliver(
        state,
        reader,
        offer,
        receive_op,
        ArgWriter::new().string(mime).build(),
    );
    read_end
}

/// Answer as the source client would: find the descriptor it was handed, write
/// to it, and read back what reaches the other end.
///
/// Which `send_op` to expect is the point of the cross-interface tests — the
/// source is told to send through *its* interface, not the reader's.
fn answer(
    owner_rx: &mut Receiver<WaylandEvent>,
    source_id: u32,
    send_op: u16,
    content: &[u8],
    read_end: OwnedFd,
) -> String {
    let mut sent = drain(owner_rx);
    let relayed = sent
        .iter_mut()
        .find(|m| m.object_id == source_id && m.op_code == send_op)
        .and_then(|m| m.fds.pop())
        .expect("the source should have been asked to send, with the pipe");
    let mut writer = std::fs::File::from(relayed);
    writer.write_all(content).unwrap();
    drop(writer);

    let mut got = String::new();
    std::fs::File::from(read_end)
        .read_to_string(&mut got)
        .unwrap();
    got
}

#[test]
fn a_manager_is_told_both_selections_the_moment_it_binds() {
    let mut state = crate::tests::test_state();
    let (mut owner_rx, _) = add_data_client(&mut state, 1);
    state.focused_surface = Some((1, super::wl_data_device::SURFACE));
    offer_selection(&mut state, 1, "text/plain", 99);
    drain(&mut owner_rx);

    let (mut manager_rx, _) = add_manager(&mut state, 2, 2);

    // A manager starts with the session and binds long after the first copy.
    // Nothing else will come along to tell it what it missed — there is no
    // focus change in its future, because it has no surface to focus.
    let sent = drain(&mut manager_rx);
    let offer = selection_offer(&sent, zwlr_data_control_device::SELECTION)
        .expect("the clipboard it missed");
    assert_eq!(mime_types_of(&sent, offer), vec!["text/plain".to_string()]);
    assert_eq!(
        selection_offer(&sent, zwlr_data_control_device::PRIMARY_SELECTION),
        None,
        "and told plainly that the other selection is empty"
    );
}

#[test]
fn a_manager_hears_about_a_selection_it_has_no_focus_for() {
    let mut state = crate::tests::test_state();
    let (_owner_rx, _) = add_data_client(&mut state, 1);
    let (mut other_rx, _) = add_data_client(&mut state, 3);
    let (mut manager_rx, _) = add_manager(&mut state, 2, 2);
    // Focus is on a third client, so neither the owner nor the manager has it.
    state.focused_surface = Some((3, super::wl_data_device::SURFACE));
    drain(&mut manager_rx);
    drain(&mut other_rx);

    offer_selection(&mut state, 1, "text/html", 99);

    let sent = drain(&mut manager_rx);
    let offer = selection_offer(&sent, zwlr_data_control_device::SELECTION)
        .expect("a manager is told regardless of focus — that is the whole interface");
    assert_eq!(mime_types_of(&sent, offer), vec!["text/html".to_string()]);
}

#[test]
fn a_manager_hears_about_the_primary_selection_too() {
    use super::zwp_primary_selection as primary;

    let mut state = crate::tests::test_state();
    let (_owner_rx, _) = primary::add_primary_client(&mut state, 1);
    let (mut manager_rx, _) = add_manager(&mut state, 2, 2);
    drain(&mut manager_rx);

    primary::offer_primary(&mut state, 1, primary::SOURCE, "text/plain");

    let sent = drain(&mut manager_rx);
    let offer = selection_offer(&sent, zwlr_data_control_device::PRIMARY_SELECTION)
        .expect("the middle-click clipboard reaches a manager as well");
    assert_eq!(mime_types_of(&sent, offer), vec!["text/plain".to_string()]);
}

#[test]
fn a_version_1_manager_is_not_sent_the_primary_selection() {
    use super::zwp_primary_selection as primary;

    let mut state = crate::tests::test_state();
    let (_owner_rx, _) = primary::add_primary_client(&mut state, 1);
    let (mut manager_rx, token) = add_manager(&mut state, 2, 1);
    drain(&mut manager_rx);

    primary::offer_primary(&mut state, 1, primary::SOURCE, "text/plain");

    // `primary_selection` arrived in version 2. Sending it to a version 1
    // device would be an opcode that object does not have.
    let sent = drain(&mut manager_rx);
    assert!(
        !sent
            .iter()
            .any(|m| m.op_code == zwlr_data_control_device::PRIMARY_SELECTION),
        "a version 1 manager has no primary selection half"
    );
    assert!(!token.is_cancelled());
}

#[test]
fn a_manager_can_take_the_clipboard_with_no_serial_and_no_focus() {
    let mut state = crate::tests::test_state();
    let (mut manager_rx, token) = add_manager(&mut state, 2, 2);
    make_source(&mut state, 2, SOURCE, "text/plain");
    drain(&mut manager_rx);

    // No serial in the request at all, and this client has never been focused
    // or sent an input event. A `wl_data_device` client doing this would be
    // refused for both reasons.
    deliver(
        &mut state,
        2,
        DEVICE,
        SET_SELECTION,
        ArgWriter::new().u32(SOURCE).build(),
    );

    assert_eq!(state.selection, Some((2, SOURCE)));
    assert!(!token.is_cancelled());
    assert!(!was_sent_an_error(&drain(&mut manager_rx)));
}

#[test]
fn a_manager_can_take_the_primary_selection_the_same_way() {
    let mut state = crate::tests::test_state();
    let (mut manager_rx, _) = add_manager(&mut state, 2, 2);
    make_source(&mut state, 2, SOURCE, "text/plain");
    drain(&mut manager_rx);

    deliver(
        &mut state,
        2,
        DEVICE,
        SET_PRIMARY_SELECTION,
        ArgWriter::new().u32(SOURCE).build(),
    );

    assert_eq!(state.primary_selection, Some((2, SOURCE)));
    assert_eq!(state.selection, None, "and leaves the clipboard alone");
}

#[test]
fn a_selection_a_manager_set_reaches_an_ordinary_client() {
    let mut state = crate::tests::test_state();
    let (mut reader_rx, _) = add_data_client(&mut state, 1);
    let (mut manager_rx, _) = add_manager(&mut state, 2, 2);
    state.focused_surface = Some((1, super::wl_data_device::SURFACE));
    drain(&mut reader_rx);

    make_source(&mut state, 2, SOURCE, "text/plain");
    deliver(
        &mut state,
        2,
        DEVICE,
        SET_SELECTION,
        ArgWriter::new().u32(SOURCE).build(),
    );
    drain(&mut manager_rx);

    // The ordinary client sees an ordinary `wl_data_offer`; nothing about it
    // says the source behind it belongs to a different interface.
    let sent = drain(&mut reader_rx);
    let offer = sent
        .iter()
        .find(|m| {
            m.object_id == super::wl_data_device::DEVICE && m.op_code == wl_data_device::DATA_OFFER
        })
        .map(|m| ArgReader::new(&m.args).u32().unwrap())
        .expect("a data_offer for the manager's selection");
    assert_eq!(mime_types_of(&sent, offer), vec!["text/plain".to_string()]);

    // And a paste from it reaches the manager through *its* send — opcode 0 on
    // zwlr_data_control_source_v1, where wl_data_source.send is opcode 1.
    let read_end = receive(&mut state, 1, offer, wl_data_offer::RECEIVE, "text/plain");
    let got = answer(
        &mut manager_rx,
        SOURCE,
        zwlr_data_control_source::SEND,
        b"restored by the manager",
        read_end,
    );
    assert_eq!(got, "restored by the manager");
}

#[test]
fn a_manager_can_read_an_ordinary_selection_before_its_owner_exits() {
    let mut state = crate::tests::test_state();
    let (mut owner_rx, _) = add_data_client(&mut state, 1);
    let (mut manager_rx, _) = add_manager(&mut state, 2, 2);
    offer_selection(&mut state, 1, "text/plain", 99);
    drain(&mut owner_rx);

    let offer = selection_offer(&drain(&mut manager_rx), zwlr_data_control_device::SELECTION)
        .expect("the manager's offer");

    // This is what the interface is for: taking a copy while the owner is still
    // alive, so the content can outlive the client that produced it. The
    // descriptor goes to a `wl_data_source`, whose send is opcode 1.
    let read_end = receive(
        &mut state,
        2,
        offer,
        tokio_way_core::protocol::zwlr_data_control_offer::RECEIVE,
        "text/plain",
    );
    let got = answer(
        &mut owner_rx,
        super::wl_data_device::SOURCE,
        wl_data_source::SEND,
        b"copied from a terminal",
        read_end,
    );
    assert_eq!(got, "copied from a terminal");
}

#[test]
fn offering_one_source_twice_is_fatal() {
    let mut state = crate::tests::test_state();
    let (mut manager_rx, token) = add_manager(&mut state, 2, 2);
    make_source(&mut state, 2, SOURCE, "text/plain");
    deliver(
        &mut state,
        2,
        DEVICE,
        SET_SELECTION,
        ArgWriter::new().u32(SOURCE).build(),
    );
    drain(&mut manager_rx);

    // Unlike the primary selection's interface, this one has an error for it.
    deliver(
        &mut state,
        2,
        DEVICE,
        SET_PRIMARY_SELECTION,
        ArgWriter::new().u32(SOURCE).build(),
    );

    assert!(was_sent_an_error(&drain(&mut manager_rx)));
    assert!(token.is_cancelled());
    assert_eq!(
        state.primary_selection, None,
        "and the second selection is not taken"
    );
}

#[test]
fn adding_a_mime_type_to_a_cancelled_source_is_fatal() {
    let mut state = crate::tests::test_state();
    let (mut manager_rx, token) = add_manager(&mut state, 2, 2);
    make_source(&mut state, 2, SOURCE, "text/plain");
    deliver(
        &mut state,
        2,
        DEVICE,
        SET_SELECTION,
        ArgWriter::new().u32(SOURCE).build(),
    );
    // A second source replaces the first, which cancels it.
    make_source(&mut state, 2, SECOND_SOURCE, "text/plain");
    deliver(
        &mut state,
        2,
        DEVICE,
        SET_SELECTION,
        ArgWriter::new().u32(SECOND_SOURCE).build(),
    );
    drain(&mut manager_rx);

    deliver(
        &mut state,
        2,
        SOURCE,
        SOURCE_OFFER,
        ArgWriter::new().string("text/html").build(),
    );

    assert!(was_sent_an_error(&drain(&mut manager_rx)));
    assert!(token.is_cancelled());
}

#[test]
fn a_source_of_another_interface_cannot_be_put_on_a_selection() {
    let mut state = crate::tests::test_state();
    let (mut client_rx, token) = add_data_client(&mut state, 1);
    // One client holding both managers, which is ordinary: a toolkit binds the
    // data device, and this test gives it a control device as well.
    state
        .clients
        .get(1)
        .unwrap()
        .register_client_object_with_version(MANAGER, ObjectType::ZwlrDataControlManager, 2)
        .unwrap();
    deliver(
        &mut state,
        1,
        MANAGER,
        GET_DATA_DEVICE,
        ArgWriter::new().u32(DEVICE).u32(0).build(),
    );
    // A plain `wl_data_source`, made through the other manager.
    deliver(
        &mut state,
        1,
        super::wl_data_device::MANAGER,
        0, // create_data_source
        ArgWriter::new().u32(super::wl_data_device::SOURCE).build(),
    );
    drain(&mut client_rx);

    // One map holds the sources of all three interfaces, so this id *is* found
    // — and has to be refused on the strength of which interface it speaks.
    deliver(
        &mut state,
        1,
        DEVICE,
        SET_SELECTION,
        ArgWriter::new().u32(super::wl_data_device::SOURCE).build(),
    );

    assert_eq!(
        state.selection, None,
        "a wl_data_source may not be handed to a data-control device"
    );
    assert!(!token.is_cancelled(), "and it is refused quietly");
    assert!(!was_sent_an_error(&drain(&mut client_rx)));
}

#[test]
fn a_manager_clears_the_clipboard_with_a_null_source() {
    let mut state = crate::tests::test_state();
    let (mut owner_rx, _) = add_data_client(&mut state, 1);
    let (mut manager_rx, _) = add_manager(&mut state, 2, 2);
    offer_selection(&mut state, 1, "text/plain", 99);
    drain(&mut manager_rx);

    deliver(
        &mut state,
        2,
        DEVICE,
        SET_SELECTION,
        ArgWriter::new().u32(0).build(),
    );

    assert_eq!(state.selection, None);
    assert!(
        drain(&mut owner_rx)
            .iter()
            .any(|m| m.object_id == super::wl_data_device::SOURCE
                && m.op_code == wl_data_source::CANCELLED),
        "the client that owned it is told it no longer does"
    );
    assert_eq!(
        selection_offer(&drain(&mut manager_rx), zwlr_data_control_device::SELECTION),
        None,
        "and the manager is told there is nothing on it"
    );
}

#[test]
fn a_manager_going_away_leaves_the_selection_it_set_empty() {
    let mut state = crate::tests::test_state();
    let (mut reader_rx, _) = add_data_client(&mut state, 1);
    let (mut manager_rx, _) = add_manager(&mut state, 2, 2);
    state.focused_surface = Some((1, super::wl_data_device::SURFACE));
    make_source(&mut state, 2, SOURCE, "text/plain");
    deliver(
        &mut state,
        2,
        DEVICE,
        SET_SELECTION,
        ArgWriter::new().u32(SOURCE).build(),
    );
    drain(&mut reader_rx);
    drain(&mut manager_rx);

    state.remove_client_resources(2);

    // A manager is as mortal as any other owner: the bytes only ever existed in
    // it, so a selection it set dies with it.
    assert_eq!(state.selection, None);
    assert!(state.data_control_devices.is_empty());
    let sent = drain(&mut reader_rx);
    let selection = sent
        .iter()
        .rev()
        .find(|m| {
            m.object_id == super::wl_data_device::DEVICE && m.op_code == wl_data_device::SELECTION
        })
        .expect("the focused client is told");
    assert_eq!(ArgReader::new(&selection.args).u32().unwrap(), 0);
}
