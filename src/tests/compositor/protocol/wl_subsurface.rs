//! Tests for synchronised subsurfaces.
//!
//! A subsurface starts synchronised, and a synchronised commit does not reach
//! the screen: it lands in a cache that is applied when the parent's own state
//! is. That is the whole point — a window and its subsurfaces update in one
//! piece instead of tearing against each other — and it is invisible from
//! outside except in *when* things change, which is exactly what these check.

use super::{CLIENT, deliver, was_sent_an_error};
use tokio::sync::mpsc::channel;
use tokio_util::sync::CancellationToken;
use tokio_way_core::protocol::{ObjectType, wire_utils::ArgWriter};
use tokio_way_core::state::CompositorState;

const PARENT: u32 = 10;
const CHILD: u32 = 11;
const GRANDCHILD: u32 = 12;
const SUBSURFACE: u32 = 20;
const CHILD_SUBSURFACE: u32 = 21;
const SUBCOMPOSITOR: u32 = 30;
const BUFFER: u32 = 40;
const OTHER_BUFFER: u32 = 41;

// wl_surface
const ATTACH: u16 = 1;
const FRAME: u16 = 3;
const COMMIT: u16 = 6;
// wl_subcompositor
const GET_SUBSURFACE: u16 = 1;
// wl_subsurface
const SET_SYNC: u16 = 4;
const SET_DESYNC: u16 = 5;

/// A parent surface with a subsurface under it, and two buffers to attach.
fn state_with_a_subsurface() -> CompositorState {
    let mut state = crate::tests::test_state();
    let (tx, _rx) = channel(256);
    state.clients.create(CLIENT, tx, CancellationToken::new());
    let client = state.clients.get(CLIENT).unwrap();
    client
        .register_client_object(SUBCOMPOSITOR, ObjectType::WlSubcompositor)
        .unwrap();
    for surface in [PARENT, CHILD, GRANDCHILD] {
        client
            .register_client_object(surface, ObjectType::WlSurface)
            .unwrap();
    }
    for buffer in [BUFFER, OTHER_BUFFER] {
        client
            .register_client_object(buffer, ObjectType::WlBuffer)
            .unwrap();
    }
    for surface in [PARENT, CHILD, GRANDCHILD] {
        state.create_surface(CLIENT, surface);
    }

    get_subsurface(&mut state, SUBSURFACE, CHILD, PARENT);
    state
}

fn get_subsurface(state: &mut CompositorState, id: u32, surface: u32, parent: u32) {
    deliver(
        state,
        SUBCOMPOSITOR,
        GET_SUBSURFACE,
        ArgWriter::new().u32(id).u32(surface).u32(parent).build(),
    );
}

fn attach(state: &mut CompositorState, surface: u32, buffer: u32) {
    deliver(
        state,
        surface,
        ATTACH,
        ArgWriter::new().u32(buffer).i32(0).i32(0).build(),
    );
}

fn commit(state: &mut CompositorState, surface: u32) {
    deliver(state, surface, COMMIT, Vec::new());
}

/// `wl_surface.frame` allocates the callback id itself, so the test must not
/// register it first — doing so is a reused id, which is fatal.
fn frame(state: &mut CompositorState, surface: u32, callback: u32) {
    deliver(
        state,
        surface,
        FRAME,
        ArgWriter::new().u32(callback).build(),
    );
}

fn attached_buffer(state: &CompositorState, surface: u32) -> Option<u32> {
    state.surfaces[&(CLIENT, surface)].buffer_id
}

#[test]
fn a_subsurface_starts_synchronised() {
    // The protocol requires it: a subsurface is created to be part of its
    // parent's next frame, not to appear on its own before the parent has said
    // where it goes.
    let state = state_with_a_subsurface();
    assert!(state.surfaces[&(CLIENT, CHILD)].is_subsurface);
    assert!(state.is_effectively_synced((CLIENT, CHILD)));
    assert!(
        !state.is_effectively_synced((CLIENT, PARENT)),
        "a toplevel has no commit mode at all",
    );
}

#[test]
fn a_synchronised_commit_waits_for_the_parent() {
    let mut state = state_with_a_subsurface();

    attach(&mut state, CHILD, BUFFER);
    commit(&mut state, CHILD);

    assert_eq!(
        attached_buffer(&state, CHILD),
        None,
        "a synchronised commit must not reach the screen on its own",
    );
    assert!(
        state.surfaces[&(CLIENT, CHILD)].cached.is_some(),
        "it must be waiting in the cache",
    );

    commit(&mut state, PARENT);

    assert_eq!(
        attached_buffer(&state, CHILD),
        Some(BUFFER),
        "the parent's commit is what applies it",
    );
    assert!(state.surfaces[&(CLIENT, CHILD)].cached.is_none());
}

#[test]
fn commits_made_while_synchronised_accumulate() {
    // A client may commit any number of times before its parent does, and the
    // protocol says the cache accumulates rather than being replaced. Every
    // frame callback is still owed its `done`, and the last buffer attached is
    // the one that shows.
    let mut state = state_with_a_subsurface();

    attach(&mut state, CHILD, BUFFER);
    frame(&mut state, CHILD, 100);
    commit(&mut state, CHILD);

    attach(&mut state, CHILD, OTHER_BUFFER);
    frame(&mut state, CHILD, 101);
    commit(&mut state, CHILD);

    commit(&mut state, PARENT);

    assert_eq!(
        attached_buffer(&state, CHILD),
        Some(OTHER_BUFFER),
        "the newer attach wins",
    );
    assert_eq!(
        state.surfaces[&(CLIENT, CHILD)].frame_callbacks,
        vec![100, 101],
        "but both callbacks are owed, oldest first",
    );
}

#[test]
fn a_desynced_subsurface_applies_its_own_commits() {
    let mut state = state_with_a_subsurface();
    deliver(&mut state, SUBSURFACE, SET_DESYNC, Vec::new());
    assert!(!state.is_effectively_synced((CLIENT, CHILD)));

    attach(&mut state, CHILD, BUFFER);
    commit(&mut state, CHILD);

    assert_eq!(
        attached_buffer(&state, CHILD),
        Some(BUFFER),
        "desynced, so nothing waits for the parent",
    );
}

#[test]
fn desyncing_flushes_what_was_cached_while_synchronised() {
    // The protocol says the cached state is applied immediately on desync.
    // Leaving it in the cache would strand a commit the client has already
    // made, with nothing later obliged to flush it.
    let mut state = state_with_a_subsurface();

    attach(&mut state, CHILD, BUFFER);
    commit(&mut state, CHILD);
    assert_eq!(attached_buffer(&state, CHILD), None);

    deliver(&mut state, SUBSURFACE, SET_DESYNC, Vec::new());

    assert_eq!(
        attached_buffer(&state, CHILD),
        Some(BUFFER),
        "desyncing applies what was waiting",
    );
    assert!(state.surfaces[&(CLIENT, CHILD)].cached.is_none());
}

#[test]
fn synchronisation_is_inherited_down_the_tree() {
    // A subsurface under a synchronised subsurface stays effectively
    // synchronised however it sets its own mode. That is what lets a client
    // put a whole subtree into one frame by synchronising its root.
    let mut state = state_with_a_subsurface();
    get_subsurface(&mut state, CHILD_SUBSURFACE, GRANDCHILD, CHILD);
    deliver(&mut state, CHILD_SUBSURFACE, SET_DESYNC, Vec::new());

    assert!(
        state.is_effectively_synced((CLIENT, GRANDCHILD)),
        "its parent is synchronised, so desyncing itself changes nothing",
    );

    attach(&mut state, GRANDCHILD, BUFFER);
    commit(&mut state, GRANDCHILD);
    assert_eq!(
        attached_buffer(&state, GRANDCHILD),
        None,
        "so its commit still waits",
    );

    // The cascade: the parent's commit applies the child, and applying the
    // child is the moment the grandchild's cache applies too.
    commit(&mut state, PARENT);
    assert_eq!(attached_buffer(&state, GRANDCHILD), Some(BUFFER));
}

#[test]
fn desyncing_the_middle_of_a_tree_frees_what_is_below_it() {
    let mut state = state_with_a_subsurface();
    get_subsurface(&mut state, CHILD_SUBSURFACE, GRANDCHILD, CHILD);
    deliver(&mut state, CHILD_SUBSURFACE, SET_DESYNC, Vec::new());

    // Desyncing the middle surface makes the already-desynced one below it
    // effectively desynced as well.
    deliver(&mut state, SUBSURFACE, SET_DESYNC, Vec::new());
    assert!(!state.is_effectively_synced((CLIENT, GRANDCHILD)));

    attach(&mut state, GRANDCHILD, BUFFER);
    commit(&mut state, GRANDCHILD);
    assert_eq!(attached_buffer(&state, GRANDCHILD), Some(BUFFER));
}

#[test]
fn re_syncing_holds_the_next_commit_again() {
    let mut state = state_with_a_subsurface();
    deliver(&mut state, SUBSURFACE, SET_DESYNC, Vec::new());
    attach(&mut state, CHILD, BUFFER);
    commit(&mut state, CHILD);
    assert_eq!(attached_buffer(&state, CHILD), Some(BUFFER));

    deliver(&mut state, SUBSURFACE, SET_SYNC, Vec::new());
    attach(&mut state, CHILD, OTHER_BUFFER);
    commit(&mut state, CHILD);

    assert_eq!(
        attached_buffer(&state, CHILD),
        Some(BUFFER),
        "synchronised again, so the new buffer waits",
    );
    commit(&mut state, PARENT);
    assert_eq!(attached_buffer(&state, CHILD), Some(OTHER_BUFFER));
}

#[test]
fn a_popup_is_not_a_subsurface_and_is_never_held_back() {
    // Popups are parented through the same two fields subsurfaces use, so
    // asking "does it have a parent" instead of "is it a subsurface" would put
    // every popup into a cache it never leaves — an invisible menu.
    let mut state = state_with_a_subsurface();
    let child = state.surfaces.get_mut(&(CLIENT, GRANDCHILD)).unwrap();
    child.parent = Some(PARENT);
    state
        .surfaces
        .get_mut(&(CLIENT, PARENT))
        .unwrap()
        .children
        .push(GRANDCHILD);

    assert!(!state.is_effectively_synced((CLIENT, GRANDCHILD)));

    attach(&mut state, GRANDCHILD, BUFFER);
    commit(&mut state, GRANDCHILD);
    assert_eq!(attached_buffer(&state, GRANDCHILD), Some(BUFFER));
}

#[test]
fn an_ordinary_toplevel_commit_still_applies_at_once() {
    // The change must not have made every surface wait for something.
    let mut state = state_with_a_subsurface();
    attach(&mut state, PARENT, BUFFER);
    commit(&mut state, PARENT);
    assert_eq!(attached_buffer(&state, PARENT), Some(BUFFER));
}

#[test]
fn destroying_the_subsurface_gives_the_surface_back() {
    // The wl_surface outlives the wl_subsurface and becomes an ordinary
    // surface again. Leaving the role set would leave every later commit going
    // into a cache with no parent left to apply it — a surface that never
    // appears again, from a client that did nothing wrong.
    const DESTROY: u16 = 0;
    let mut state = state_with_a_subsurface();

    attach(&mut state, CHILD, BUFFER);
    commit(&mut state, CHILD);
    assert_eq!(attached_buffer(&state, CHILD), None, "cached while synced");

    deliver(&mut state, SUBSURFACE, DESTROY, Vec::new());

    assert!(!state.surfaces[&(CLIENT, CHILD)].is_subsurface);
    assert!(!state.is_effectively_synced((CLIENT, CHILD)));
    assert_eq!(
        attached_buffer(&state, CHILD),
        Some(BUFFER),
        "and what was cached is applied rather than stranded",
    );

    // And it goes on working as a plain surface.
    attach(&mut state, CHILD, OTHER_BUFFER);
    commit(&mut state, CHILD);
    assert_eq!(attached_buffer(&state, CHILD), Some(OTHER_BUFFER));
}

#[test]
fn setting_the_mode_is_never_an_error() {
    let mut state = state_with_a_subsurface();
    let (tx, mut rx) = channel(64);
    state.clients.get(CLIENT).unwrap().sender = tx;

    deliver(&mut state, SUBSURFACE, SET_SYNC, Vec::new());
    deliver(&mut state, SUBSURFACE, SET_DESYNC, Vec::new());
    deliver(&mut state, SUBSURFACE, SET_DESYNC, Vec::new());

    assert!(!was_sent_an_error(&mut rx));
}
