//! Tests for `wp_cursor_shape_v1`: the shape table, the serial rule, and the
//! fact that naming a shape and attaching a cursor surface are one choice made
//! two ways.
//!
//! Nothing here reads the machine's cursor theme. A test that did would pass or
//! fail on which theme happens to be installed, so the images are seeded into
//! the compositor's cache and the loader is tested separately with a name no
//! theme has.

use super::{CLIENT, SURFACE, deliver};
use tokio::sync::mpsc::{Receiver, channel};
use tokio_util::sync::CancellationToken;
use tokio_way_core::protocol::wire_utils::ArgWriter;
use tokio_way_core::protocol::wp_cursor_shape_device::names_for;
use tokio_way_core::protocol::{ObjectType, wl_display};
use tokio_way_core::scene::load_theme_cursor;
use tokio_way_core::state::{CompositorState, DefaultCursor};
use tokio_way_sock::WaylandEvent;

const MANAGER: u32 = 50;
const DEVICE: u32 = 51;
const POINTER: u32 = 52;
const SERIAL: u32 = 77;

// Shape numbers from the protocol's own enum.
const SHAPE_DEFAULT: u32 = 1;
const SHAPE_TEXT: u32 = 9;
/// One past the last shape version 1 defines.
const SHAPE_PAST_THE_END: u32 = 35;

// Request opcodes.
const GET_POINTER: u16 = 1;
const SET_SHAPE: u16 = 1;
const SET_CURSOR: u16 = 0;

/// A client holding a cursor shape device, with the pointer over one of its
/// surfaces and an enter serial it can quote.
fn state_with_a_device() -> (CompositorState, Receiver<WaylandEvent>, CancellationToken) {
    let mut state = crate::tests::test_state();
    let (tx, rx) = channel(64);
    let token = CancellationToken::new();
    state.clients.create(CLIENT, tx, token.clone());
    let client = state.clients.get(CLIENT).unwrap();
    client
        .register(MANAGER, ObjectType::WpCursorShapeManager)
        .unwrap();
    client.register(POINTER, ObjectType::WlPointer).unwrap();
    client.register(SURFACE, ObjectType::WlSurface).unwrap();
    state.create_surface(CLIENT, SURFACE);
    state.pointer_surface = Some((CLIENT, SURFACE));
    state.pointer_enter_serial.insert(CLIENT, SERIAL);

    deliver(
        &mut state,
        MANAGER,
        GET_POINTER,
        ArgWriter::new().u32(DEVICE).u32(POINTER).build(),
    );
    (state, rx, token)
}

/// Pretend the theme has a cursor for this shape, without touching the theme.
fn seed_shape(state: &mut CompositorState, shape: u32) {
    state.cursor_shape_images.insert(
        names_for(shape).unwrap(),
        DefaultCursor {
            pixels: vec![0xffff_ffff; 4],
            width: 2,
            height: 2,
            hotspot_x: 1,
            hotspot_y: 1,
        },
    );
}

fn set_shape(state: &mut CompositorState, serial: u32, shape: u32) {
    deliver(
        state,
        DEVICE,
        SET_SHAPE,
        ArgWriter::new().u32(serial).u32(shape).build(),
    );
}

fn was_sent_an_error(rx: &mut Receiver<WaylandEvent>) -> bool {
    std::iter::from_fn(|| rx.try_recv().ok())
        .any(|m| m.object_id == wl_display::OBJECT_ID && m.op_code == wl_display::ERROR)
}

#[test]
fn every_shape_the_interface_defines_has_a_name_to_look_up() {
    // Version 1 numbers its shapes 1 to 34. Zero is not a shape, and 35 is the
    // first one version 2 added.
    assert!(names_for(0).is_none());
    for shape in 1..=34 {
        let names = names_for(shape).unwrap_or_else(|| panic!("shape {shape} has no names"));
        assert!(!names.is_empty(), "shape {shape} has an empty name list");
    }
    assert!(
        names_for(SHAPE_PAST_THE_END).is_none(),
        "advertising version 1 means refusing version 2's shapes"
    );
}

#[test]
fn naming_a_shape_sets_the_cursor() {
    let (mut state, mut rx, token) = state_with_a_device();
    seed_shape(&mut state, SHAPE_TEXT);

    set_shape(&mut state, SERIAL, SHAPE_TEXT);

    assert_eq!(
        state.cursor_shapes.get(&CLIENT),
        names_for(SHAPE_TEXT).as_ref()
    );
    assert!(state.cursor_dirty, "and the cursor is recomposed");
    assert!(!was_sent_an_error(&mut rx));
    assert!(!token.is_cancelled());
}

#[test]
fn a_serial_that_is_not_the_current_enter_is_ignored() {
    let (mut state, mut rx, token) = state_with_a_device();
    seed_shape(&mut state, SHAPE_TEXT);

    set_shape(&mut state, SERIAL + 1, SHAPE_TEXT);

    // The same rule `wl_pointer.set_cursor` follows, and the same answer: the
    // client lost a race it could not see, and that is not worth a disconnect.
    assert!(state.cursor_shapes.is_empty());
    assert!(!was_sent_an_error(&mut rx));
    assert!(!token.is_cancelled());
}

#[test]
fn a_shape_the_interface_does_not_define_is_fatal() {
    let (mut state, mut rx, token) = state_with_a_device();

    set_shape(&mut state, SERIAL, SHAPE_PAST_THE_END);

    assert!(was_sent_an_error(&mut rx));
    assert!(token.is_cancelled());
}

#[test]
fn whichever_way_the_cursor_was_set_last_wins() {
    let (mut state, _rx, _) = state_with_a_device();
    seed_shape(&mut state, SHAPE_DEFAULT);

    // A shape after a surface.
    state.cursor_surfaces.insert(CLIENT, Some((SURFACE, 0, 0)));
    set_shape(&mut state, SERIAL, SHAPE_DEFAULT);
    assert!(
        !state.cursor_surfaces.contains_key(&CLIENT),
        "the surface must stop being drawn"
    );

    // And a surface after a shape: hiding the cursor is a `set_cursor` too.
    deliver(
        &mut state,
        POINTER,
        SET_CURSOR,
        ArgWriter::new().u32(SERIAL).u32(0).i32(0).i32(0).build(),
    );
    assert!(
        state.cursor_shapes.is_empty(),
        "the shape must stop being drawn"
    );
    assert_eq!(state.cursor_surfaces.get(&CLIENT), Some(&None));
}

#[test]
fn a_client_going_away_takes_its_shape_with_it() {
    let (mut state, _rx, _) = state_with_a_device();
    seed_shape(&mut state, SHAPE_TEXT);
    set_shape(&mut state, SERIAL, SHAPE_TEXT);

    state.remove_client_resources(CLIENT);

    assert!(state.cursor_shapes.is_empty());
    assert!(
        !state.cursor_shape_images.is_empty(),
        "but the image stays loaded: it is the theme's, not the client's"
    );
}

#[test]
fn a_cursor_no_theme_has_does_not_load() {
    // The loader's miss path, with a name no theme could have. Everything else
    // here seeds the cache instead of reading the disk, so this is the one test
    // that touches a real theme — and it is hermetic because it asks for
    // something that cannot exist.
    assert!(load_theme_cursor(&["way-small-no-such-cursor-cbf1"]).is_none());
}
