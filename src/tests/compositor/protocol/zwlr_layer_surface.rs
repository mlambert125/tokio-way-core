//! Tests for a popup opened from a layer surface.
//!
//! This is the sequence a bar performs to show a menu or a tooltip, and it is
//! not the one a window performs. The popup is created with a *null* parent —
//! the layer shell says so outright, "created via `xdg_surface::get_popup` with
//! the parent set to NULL" — and `zwlr_layer_surface_v1.get_popup` is what
//! then gives it one.
//!
//! Worth its own file because it crosses two protocols, and because refusing a
//! null parent looks entirely reasonable until layer shell exists: it was
//! exactly right when `xdg_shell` was the only way to make a popup, and became
//! a fatal error for every bar the moment it was not.

use super::{CLIENT, deliver, was_sent_an_error};
use tokio::sync::mpsc::{Receiver, channel};
use tokio_util::sync::CancellationToken;
use tokio_way_backends::outputs::OutputId;
use tokio_way_core::protocol::{ObjectType, wire_utils::ArgWriter};
use tokio_way_core::state::CompositorState;
use tokio_way_sock::WaylandEvent;

const LAYER_SHELL: u32 = 5;
const BAR_SURFACE: u32 = 10;
const BAR: u32 = 11;
const POPUP_SURFACE: u32 = 20;
const POPUP_XDG_SURFACE: u32 = 21;
const POPUP: u32 = 22;
const POSITIONER: u32 = 30;

// zwlr_layer_shell_v1
const GET_LAYER_SURFACE: u16 = 0;
// zwlr_layer_surface_v1
const LAYER_SET_SIZE: u16 = 0;
const LAYER_SET_ANCHOR: u16 = 1;
const LAYER_GET_POPUP: u16 = 5;
// xdg_surface
const GET_POPUP: u16 = 2;
// wl_surface
const COMMIT: u16 = 6;

/// A client with a bar on the top layer, as a shell would have.
fn client_with_a_bar() -> (CompositorState, CancellationToken, Receiver<WaylandEvent>) {
    let mut state = crate::tests::test_state();
    let (tx, rx) = channel(256);
    let token = CancellationToken::new();
    state.clients.create(CLIENT, tx, token.clone());
    state.outputs.push(super::super::test_output(OutputId(1)));
    crate::tests::with_ws(&mut state, |w, s| w.workspaces.sync_outputs(&s.outputs));

    let client = state.clients.get(CLIENT).unwrap();
    client
        .register_client_object(LAYER_SHELL, ObjectType::ZwlrLayerShell)
        .unwrap();
    for surface in [BAR_SURFACE, POPUP_SURFACE] {
        client
            .register_client_object(surface, ObjectType::WlSurface)
            .unwrap();
    }
    client
        .register_client_object_with_version(POPUP_XDG_SURFACE, ObjectType::XdgSurface, 5)
        .unwrap();
    client
        .register_client_object(POSITIONER, ObjectType::XdgPositioner)
        .unwrap();
    for surface in [BAR_SURFACE, POPUP_SURFACE] {
        state.create_surface(CLIENT, surface);
    }
    state.create_xdg_surface(CLIENT, POPUP_XDG_SURFACE, POPUP_SURFACE);
    state.create_xdg_positioner(CLIENT, POSITIONER);

    // A bar across the top of the output.
    deliver(
        &mut state,
        LAYER_SHELL,
        GET_LAYER_SURFACE,
        ArgWriter::new()
            .u32(BAR)
            .u32(BAR_SURFACE)
            .u32(0) // let the compositor choose the output
            .u32(2) // top layer
            .string("bar")
            .build(),
    );
    deliver(
        &mut state,
        BAR,
        LAYER_SET_ANCHOR,
        ArgWriter::new().u32(1 | 4 | 8).build(),
    );
    deliver(
        &mut state,
        BAR,
        LAYER_SET_SIZE,
        ArgWriter::new().u32(0).u32(30).build(),
    );
    deliver(&mut state, BAR_SURFACE, COMMIT, Vec::new());

    (state, token, rx)
}

/// `xdg_surface.get_popup(popup, parent, positioner)`.
fn get_popup(state: &mut CompositorState, parent: u32) {
    deliver(
        state,
        POPUP_XDG_SURFACE,
        GET_POPUP,
        ArgWriter::new()
            .u32(POPUP)
            .u32(parent)
            .u32(POSITIONER)
            .build(),
    );
}

#[test]
fn a_popup_may_be_created_with_no_parent_yet() {
    // What every bar does. Refusing it disconnected the client, which took the
    // whole shell down with it — a tooltip is not worth a desktop.
    let (mut state, token, mut rx) = client_with_a_bar();

    get_popup(&mut state, 0);

    assert!(!was_sent_an_error(&mut rx), "a null parent is legal here");
    assert!(!token.is_cancelled());
    assert!(state.xdg_popups.contains_key(&(CLIENT, POPUP)));
    assert_eq!(
        state.surfaces[&(CLIENT, POPUP_SURFACE)].parent,
        None,
        "it has no parent until the layer surface gives it one",
    );
}

#[test]
fn a_layer_surface_adopts_the_popup_and_places_it() {
    let (mut state, token, mut rx) = client_with_a_bar();
    get_popup(&mut state, 0);

    deliver(
        &mut state,
        BAR,
        LAYER_GET_POPUP,
        ArgWriter::new().u32(POPUP).build(),
    );

    assert!(!was_sent_an_error(&mut rx));
    assert!(!token.is_cancelled());
    assert_eq!(
        state.surfaces[&(CLIENT, POPUP_SURFACE)].parent,
        Some(BAR_SURFACE),
        "the popup now hangs off the bar",
    );
    assert_eq!(
        state.surfaces[&(CLIENT, BAR_SURFACE)].children,
        vec![POPUP_SURFACE],
        "and the bar knows about it",
    );
}

#[test]
fn a_named_parent_that_does_not_exist_is_still_refused() {
    // The check that broke bars was right about this case, and still is: a
    // parent that was named and is not there would leave the popup placed
    // against nothing.
    let (mut state, token, mut rx) = client_with_a_bar();

    get_popup(&mut state, 999);

    assert!(was_sent_an_error(&mut rx));
    assert!(token.is_cancelled());
    assert!(!state.xdg_popups.contains_key(&(CLIENT, POPUP)));
}

#[test]
fn a_bar_carries_its_placed_position_on_its_surface() {
    // Popup placement, `global_position_of` and the tree walks all read
    // `Surface::position`. A layer surface that left it at the origin would
    // have its menus placed at the top-left of the screen rather than beside
    // the panel they came from.
    let (state, _token, _rx) = client_with_a_bar();
    let bar = &state.surfaces[&(CLIENT, BAR_SURFACE)];
    assert_eq!(
        bar.position,
        (0, 0),
        "anchored to the top-left corner of this output",
    );
    assert!(
        state.layer_surfaces[&(CLIENT, BAR)]
            .configured_size
            .is_some(),
        "and it has been configured with a size",
    );
}

#[test]
fn a_popup_cannot_be_adopted_into_its_own_ancestry() {
    // The cycle check has to guard this path too: composing and hit-testing
    // recurse the surface tree, so a loop here is a blown stack rather than a
    // misplaced menu.
    let (mut state, _token, _rx) = client_with_a_bar();
    get_popup(&mut state, 0);
    deliver(
        &mut state,
        BAR,
        LAYER_GET_POPUP,
        ArgWriter::new().u32(POPUP).build(),
    );

    // Ask again, now that the bar is already below the popup in the tree.
    deliver(
        &mut state,
        BAR,
        LAYER_GET_POPUP,
        ArgWriter::new().u32(POPUP).build(),
    );

    let bar = &state.surfaces[&(CLIENT, BAR_SURFACE)];
    assert_eq!(
        bar.children,
        vec![POPUP_SURFACE],
        "the second adoption must not link it twice",
    );
    assert!(
        state.surfaces.keys().all(|&(client_id, surface_id)| {
            state.surfaces[&(client_id, surface_id)]
                .parent
                .is_none_or(|parent| !state.is_ancestor(client_id, surface_id, parent))
        }),
        "and no surface may be its own ancestor",
    );
}
