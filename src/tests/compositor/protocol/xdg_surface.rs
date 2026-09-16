//! Tests for `xdg_surface`, and in particular for what it refuses.
//!
//! `get_popup` is one of the two requests that can link one surface under
//! another, and the surface tree is recursed to compose and to hit-test. A
//! popup that ends up in its own ancestry is therefore not a misplaced window
//! but a blown stack that takes every client down with it.

use super::{CLIENT, POSITIONER, SURFACE, deliver, was_sent_an_error};
use tokio::sync::mpsc::{Receiver, channel};
use tokio_util::sync::CancellationToken;
use tokio_way_core::protocol::{ObjectType, wire_utils::ArgWriter};
use tokio_way_core::state::CompositorState;
use tokio_way_sock::WaylandEvent;

const XDG_SURFACE: u32 = 30;
const OTHER_SURFACE: u32 = 11;
const OTHER_XDG_SURFACE: u32 = 31;
const POPUP: u32 = 40;

// xdg_surface.get_popup
const GET_POPUP: u16 = 2;

/// A client with two `wl_surface`s, each wrapped in an `xdg_surface`, and a
/// positioner to place a popup with.
fn client_with_two_xdg_surfaces() -> (CompositorState, CancellationToken, Receiver<WaylandEvent>) {
    let mut state = crate::tests::test_state();
    let (tx, rx) = channel(64);
    let token = CancellationToken::new();
    state.clients.create(CLIENT, tx, token.clone());

    for (surface, xdg_surface) in [(SURFACE, XDG_SURFACE), (OTHER_SURFACE, OTHER_XDG_SURFACE)] {
        state.create_surface(CLIENT, surface);
        state.create_xdg_surface(CLIENT, xdg_surface, surface);
        let client = state.clients.get(CLIENT).unwrap();
        client.register(surface, ObjectType::WlSurface).unwrap();
        client
            .register(xdg_surface, ObjectType::XdgSurface)
            .unwrap();
    }

    state.create_xdg_positioner(CLIENT, POSITIONER);
    state
        .clients
        .get(CLIENT)
        .unwrap()
        .register(POSITIONER, ObjectType::XdgPositioner)
        .unwrap();

    (state, token, rx)
}

fn get_popup(state: &mut CompositorState, on: u32, popup_id: u32, parent: u32) {
    deliver(
        state,
        on,
        GET_POPUP,
        ArgWriter::new()
            .u32(popup_id)
            .u32(parent)
            .u32(POSITIONER)
            .build(),
    );
}

/// Whether any surface is its own ancestor. The invariant the whole check
/// exists to keep: composing and hit-testing recurse the tree, so a cycle
/// anywhere in it is a stack overflow rather than a wrong picture.
fn tree_is_acyclic(state: &CompositorState) -> bool {
    state.surfaces.keys().all(|&(client_id, surface_id)| {
        let parent = state.surfaces[&(client_id, surface_id)].parent;
        parent.is_none_or(|parent| !state.is_ancestor(client_id, surface_id, parent))
    })
}

#[test]
fn a_popup_may_not_be_its_own_parent() {
    let (mut state, token, mut rx) = client_with_two_xdg_surfaces();

    // The client names its own xdg_surface as the popup's parent. Two
    // requests, and the surface would be linked under itself.
    get_popup(&mut state, XDG_SURFACE, POPUP, XDG_SURFACE);

    assert!(was_sent_an_error(&mut rx), "the client must be told why");
    assert!(token.is_cancelled(), "and it must be disconnected");
    assert!(tree_is_acyclic(&state));
    let surface = &state.surfaces[&(CLIENT, SURFACE)];
    assert_eq!(surface.parent, None, "no half-built parent link");
    assert!(surface.children.is_empty());
}

#[test]
fn a_popup_may_not_be_parented_to_its_own_descendant() {
    let (mut state, _token, mut rx) = client_with_two_xdg_surfaces();

    // A legal popup first: the second surface hangs off the first.
    get_popup(&mut state, OTHER_XDG_SURFACE, POPUP, XDG_SURFACE);
    assert!(
        !was_sent_an_error(&mut rx),
        "that one is perfectly ordinary"
    );
    assert_eq!(
        state.surfaces[&(CLIENT, OTHER_SURFACE)].parent,
        Some(SURFACE)
    );

    // Now the other way round, which would close the loop.
    get_popup(&mut state, XDG_SURFACE, POPUP + 1, OTHER_XDG_SURFACE);

    assert!(was_sent_an_error(&mut rx));
    assert!(tree_is_acyclic(&state));
    assert_eq!(state.surfaces[&(CLIENT, SURFACE)].parent, None);
}

#[test]
fn a_popup_must_name_a_parent_that_exists() {
    let (mut state, token, mut rx) = client_with_two_xdg_surfaces();

    // 999 is not an xdg_surface this client has. Placing the popup relative to
    // nothing and leaving it unparented would give the client a window it can
    // never see; the protocol has an error for it.
    get_popup(&mut state, XDG_SURFACE, POPUP, 999);

    assert!(was_sent_an_error(&mut rx));
    assert!(token.is_cancelled());
    assert!(!state.xdg_popups.contains_key(&(CLIENT, POPUP)));
}

#[test]
fn an_ordinary_popup_is_still_accepted() {
    let (mut state, token, mut rx) = client_with_two_xdg_surfaces();

    get_popup(&mut state, OTHER_XDG_SURFACE, POPUP, XDG_SURFACE);

    assert!(!was_sent_an_error(&mut rx));
    assert!(!token.is_cancelled());
    assert!(state.xdg_popups.contains_key(&(CLIENT, POPUP)));
    assert_eq!(
        state.surfaces[&(CLIENT, OTHER_SURFACE)].parent,
        Some(SURFACE),
        "the popup hangs off its parent",
    );
    assert_eq!(
        state.surfaces[&(CLIENT, SURFACE)].children,
        vec![OTHER_SURFACE],
        "and the parent knows about it",
    );
}

// The configure/acknowledge handshake.

const TOPLEVEL: u32 = 50;
const BUFFER: u32 = 60;
/// A `wl_surface` with no `xdg_surface` over it.
const PLAIN: u32 = 70;

// xdg_surface
const GET_TOPLEVEL: u16 = 1;
const ACK_CONFIGURE: u16 = 4;
// wl_surface
const ATTACH: u16 = 1;
const COMMIT: u16 = 6;

/// A client with a toplevel, which is the point at which the compositor has
/// sent its first configure and is waiting to be answered.
fn client_with_a_toplevel() -> (CompositorState, CancellationToken, Receiver<WaylandEvent>) {
    let (mut state, token, rx) = client_with_two_xdg_surfaces();
    state
        .clients
        .get(CLIENT)
        .unwrap()
        .register(BUFFER, ObjectType::WlBuffer)
        .unwrap();
    deliver(
        &mut state,
        XDG_SURFACE,
        GET_TOPLEVEL,
        ArgWriter::new().u32(TOPLEVEL).build(),
    );
    (state, token, rx)
}

fn pending_configures(state: &CompositorState) -> Vec<u32> {
    state.xdg_surfaces[&(CLIENT, XDG_SURFACE)]
        .pending_configures
        .iter()
        .copied()
        .collect()
}

fn attach_and_commit(state: &mut CompositorState) {
    deliver(
        state,
        SURFACE,
        ATTACH,
        ArgWriter::new().u32(BUFFER).i32(0).i32(0).build(),
    );
    deliver(state, SURFACE, COMMIT, Vec::new());
}

#[test]
fn a_configure_is_recorded_when_it_is_sent() {
    // Every configure the compositor sends must be one the client can legally
    // acknowledge. Sending the serial and recording it are the same act.
    let (state, _token, _rx) = client_with_a_toplevel();
    // Two, as it happens: the initial configure, and a second from the
    // activation that follows because a new toplevel takes focus at once. Each
    // xdg_toplevel.configure needs an xdg_surface.configure of its own, so the
    // count is the point rather than a surprise — what matters is that every
    // serial sent is one the client can answer.
    assert!(
        !pending_configures(&state).is_empty(),
        "a new toplevel is owed at least one configure",
    );
    assert!(!state.xdg_surfaces[&(CLIENT, XDG_SURFACE)].configured);
}

#[test]
fn a_buffer_committed_before_any_configure_is_acknowledged_is_refused() {
    // The configure is where the compositor says how big the window may be and
    // what state it is in, so content committed before one is content sized
    // against nothing.
    let (mut state, token, mut rx) = client_with_a_toplevel();

    attach_and_commit(&mut state);

    assert!(was_sent_an_error(&mut rx), "the client must be told why");
    assert!(token.is_cancelled());
    assert_eq!(
        state.surfaces[&(CLIENT, SURFACE)].buffer_id,
        None,
        "and the refused commit must leave nothing half-applied",
    );
}

#[test]
fn a_buffer_is_accepted_once_a_configure_has_been_acknowledged() {
    let (mut state, token, mut rx) = client_with_a_toplevel();
    // The newest, which answers every earlier one with it.
    let serial = *pending_configures(&state).last().unwrap();

    deliver(
        &mut state,
        XDG_SURFACE,
        ACK_CONFIGURE,
        ArgWriter::new().u32(serial).build(),
    );
    assert!(state.xdg_surfaces[&(CLIENT, XDG_SURFACE)].configured);
    assert!(pending_configures(&state).is_empty(), "and it is answered");

    attach_and_commit(&mut state);

    assert!(!was_sent_an_error(&mut rx));
    assert!(!token.is_cancelled());
    assert_eq!(state.surfaces[&(CLIENT, SURFACE)].buffer_id, Some(BUFFER));
}

#[test]
fn acknowledging_a_serial_that_was_never_sent_is_fatal() {
    let (mut state, token, mut rx) = client_with_a_toplevel();
    let newest = *pending_configures(&state).last().unwrap();

    deliver(
        &mut state,
        XDG_SURFACE,
        ACK_CONFIGURE,
        ArgWriter::new().u32(newest + 1000).build(),
    );

    assert!(was_sent_an_error(&mut rx));
    assert!(token.is_cancelled());
    assert!(
        !state.xdg_surfaces[&(CLIENT, XDG_SURFACE)].configured,
        "an invented serial configures nothing",
    );
}

#[test]
fn acknowledging_a_configure_already_moved_past_is_not_fatal() {
    // A client may answer a configure the compositor has since superseded —
    // that is ordinary under a resize drag, and harmless. Only a serial newer
    // than anything sent names an event that never happened.
    let (mut state, token, mut rx) = client_with_a_toplevel();
    let first = *pending_configures(&state).last().unwrap();

    deliver(
        &mut state,
        XDG_SURFACE,
        ACK_CONFIGURE,
        ArgWriter::new().u32(first).build(),
    );
    // Answering the same one again: already drained, so stale.
    deliver(
        &mut state,
        XDG_SURFACE,
        ACK_CONFIGURE,
        ArgWriter::new().u32(first).build(),
    );

    assert!(!was_sent_an_error(&mut rx), "stale is not invented");
    assert!(!token.is_cancelled());
}

#[test]
fn acknowledging_one_configure_answers_every_earlier_one() {
    // A client that fell behind a burst answers only the most recent, which
    // the protocol allows outright — so the ones before it must be cleared
    // with it rather than left outstanding forever.
    let (mut state, _token, mut rx) = client_with_a_toplevel();
    let before = pending_configures(&state).len();
    // Two more configures, as a resize drag would send.
    for size in [(200, 100), (300, 150)] {
        tokio_way_core::protocol::xdg_toplevel::configure(
            &mut state,
            (CLIENT, TOPLEVEL),
            size.0,
            size.1,
        );
    }
    let outstanding = pending_configures(&state);
    assert_eq!(outstanding.len(), before + 2, "none of them answered yet");

    let newest = *outstanding.last().unwrap();
    deliver(
        &mut state,
        XDG_SURFACE,
        ACK_CONFIGURE,
        ArgWriter::new().u32(newest).build(),
    );

    assert!(pending_configures(&state).is_empty());
    assert!(state.xdg_surfaces[&(CLIENT, XDG_SURFACE)].configured);
    assert!(!was_sent_an_error(&mut rx));
}

#[test]
fn a_surface_with_no_xdg_role_needs_no_configure() {
    // A cursor, a drag icon and a subsurface all commit buffers and none of
    // them has a configure to wait for. Applying the rule to every surface
    // would disconnect a client for using a cursor.
    let (mut state, token, mut rx) = client_with_two_xdg_surfaces();
    state
        .clients
        .get(CLIENT)
        .unwrap()
        .register(BUFFER, ObjectType::WlBuffer)
        .unwrap();
    // A plain wl_surface, with no xdg_surface wrapping it.
    state.create_surface(CLIENT, PLAIN);
    state
        .clients
        .get(CLIENT)
        .unwrap()
        .register(PLAIN, ObjectType::WlSurface)
        .unwrap();

    deliver(
        &mut state,
        PLAIN,
        ATTACH,
        ArgWriter::new().u32(BUFFER).i32(0).i32(0).build(),
    );
    deliver(&mut state, PLAIN, COMMIT, Vec::new());

    assert!(!was_sent_an_error(&mut rx));
    assert!(!token.is_cancelled());
    assert_eq!(state.surfaces[&(CLIENT, PLAIN)].buffer_id, Some(BUFFER));
}
