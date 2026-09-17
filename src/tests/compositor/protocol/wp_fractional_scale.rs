//! Tests for `wp_fractional_scale_v1`: what scale a surface is told to draw at,
//! and when it is told again.

use super::{CLIENT, SURFACE, deliver};
use tokio::sync::mpsc::{Receiver, channel};
use tokio_util::sync::CancellationToken;
use tokio_way_backends::outputs::{OutputId, Scale};
use tokio_way_core::input::update_surface_outputs;
use tokio_way_core::protocol::wire_utils::{ArgReader, ArgWriter};
use tokio_way_core::protocol::{ObjectType, wl_display, wp_fractional_scale};
use tokio_way_core::state::CompositorState;
use tokio_way_sock::WaylandEvent;

const MANAGER: u32 = 40;
const OBJECT: u32 = 41;
const SECOND_OBJECT: u32 = 42;

// Request opcodes. `destroy` leads in this manager.
const GET_FRACTIONAL_SCALE: u16 = 1;

/// A client with the manager bound and one mapped window over the first output.
///
/// The window needs a buffer and a position: which outputs a surface is on is
/// worked out from geometry, and a surface with no buffer is on none of them —
/// which is a case of its own, tested separately.
fn state_with_a_window() -> (CompositorState, Receiver<WaylandEvent>, CancellationToken) {
    use tokio_way_core::state::{Buffer, BufferKind, ShmBuffer};

    let mut state = super::super::state_with_two_outputs();
    let (tx, rx) = channel(64);
    let token = CancellationToken::new();
    state.clients.create(CLIENT, tx, token.clone());
    state
        .clients
        .get(CLIENT)
        .unwrap()
        .register_client_object(MANAGER, ObjectType::WpFractionalScaleManager)
        .unwrap();
    super::super::add_toplevel(&mut state, CLIENT, SURFACE, 21, 22);
    state.buffers.insert(
        (CLIENT, 11),
        Buffer {
            client_id: CLIENT,
            width: 20,
            height: 20,
            content_serial: 1,
            kind: BufferKind::Shm(ShmBuffer {
                pool_id: 0,
                offset: 0,
                stride: 80,
                format: 0,
                damage: None,
            }),
        },
    );
    let surface = state.surfaces.get_mut(&(CLIENT, SURFACE)).unwrap();
    surface.buffer_id = Some(11);
    surface.position = (10, 10);
    update_surface_outputs(&mut state);
    (state, rx, token)
}

/// Put the window over the other output. Which output a surface is on follows
/// its geometry, the same way `wl_surface.enter` does, so moving it means moving
/// it — the two outputs sit side by side at x=0 and x=100.
fn move_over_second_output(state: &mut CompositorState) {
    if let Some(surface) = state.surfaces.get_mut(&(CLIENT, SURFACE)) {
        surface.position = (110, 10);
    }
}

/// Set the scale of one output, as a config override or a hotplug would.
fn set_output_scale(state: &mut CompositorState, output: OutputId, factor: f64) {
    if let Some(output) = state.outputs.iter_mut().find(|o| o.id == output) {
        output.scale = Scale::from_f64(factor);
    }
}

fn get_fractional_scale(state: &mut CompositorState, object: u32, surface: u32) {
    deliver(
        state,
        MANAGER,
        GET_FRACTIONAL_SCALE,
        ArgWriter::new().u32(object).u32(surface).build(),
    );
}

/// Every scale the client has been told, in 120ths.
fn scales(rx: &mut Receiver<WaylandEvent>) -> Vec<u32> {
    std::iter::from_fn(|| rx.try_recv().ok())
        .filter(|m| m.object_id == OBJECT && m.op_code == wp_fractional_scale::PREFERRED_SCALE)
        .map(|m| ArgReader::new(&m.args).u32().unwrap())
        .collect()
}

fn was_sent_an_error(rx: &mut Receiver<WaylandEvent>) -> bool {
    std::iter::from_fn(|| rx.try_recv().ok())
        .any(|m| m.object_id == wl_display::OBJECT_ID && m.op_code == wl_display::ERROR)
}

#[test]
fn the_scale_is_sent_as_soon_as_the_object_is_made() {
    let (mut state, mut rx, _) = state_with_a_window();
    set_output_scale(&mut state, OutputId(1), 1.5);

    get_fractional_scale(&mut state, OBJECT, SURFACE);

    // A client asks before its first commit, so that the first frame it draws is
    // the right size. Waiting for the frame pass would mean every client drew
    // one frame at 1× and threw it away.
    assert_eq!(scales(&mut rx), vec![180], "1.5x is 180 of 120ths");
}

#[test]
fn a_surface_on_no_output_is_told_what_the_output_it_would_open_on_says() {
    let mut state = super::super::state_with_two_outputs();
    let (tx, mut rx) = channel(64);
    state.clients.create(CLIENT, tx, CancellationToken::new());
    state
        .clients
        .get(CLIENT)
        .unwrap()
        .register_client_object(MANAGER, ObjectType::WpFractionalScaleManager)
        .unwrap();
    // A surface with no buffer and no window: on no output at all.
    state.create_surface(CLIENT, SURFACE);
    set_output_scale(&mut state, OutputId(1), 2.0);

    get_fractional_scale(&mut state, OBJECT, SURFACE);

    assert_eq!(
        scales(&mut rx),
        vec![240],
        "a guess, but a better one than 1x on a HiDPI display"
    );
}

#[test]
fn an_unchanged_scale_is_not_repeated() {
    let (mut state, mut rx, _) = state_with_a_window();
    set_output_scale(&mut state, OutputId(1), 2.0);
    get_fractional_scale(&mut state, OBJECT, SURFACE);
    assert_eq!(scales(&mut rx), vec![240]);

    update_surface_outputs(&mut state);
    update_surface_outputs(&mut state);

    // A client re-renders on every one of these, so saying the same thing twice
    // costs it a frame.
    assert!(scales(&mut rx).is_empty());
}

#[test]
fn the_scale_follows_the_output_the_surface_is_on() {
    let (mut state, mut rx, _) = state_with_a_window();
    set_output_scale(&mut state, OutputId(1), 1.0);
    set_output_scale(&mut state, OutputId(2), 2.0);
    get_fractional_scale(&mut state, OBJECT, SURFACE);
    assert_eq!(scales(&mut rx), vec![120]);

    move_over_second_output(&mut state);
    update_surface_outputs(&mut state);

    assert_eq!(
        scales(&mut rx),
        vec![240],
        "the window moved to a 2x display"
    );
}

#[test]
fn an_output_rescaled_under_a_surface_tells_it_so() {
    let (mut state, mut rx, _) = state_with_a_window();
    get_fractional_scale(&mut state, OBJECT, SURFACE);
    assert_eq!(scales(&mut rx), vec![120]);

    set_output_scale(&mut state, OutputId(1), 1.25);
    update_surface_outputs(&mut state);

    assert_eq!(scales(&mut rx), vec![150]);
}

#[test]
fn a_second_object_for_one_surface_is_fatal() {
    let (mut state, mut rx, token) = state_with_a_window();
    get_fractional_scale(&mut state, OBJECT, SURFACE);
    let _ = scales(&mut rx);

    get_fractional_scale(&mut state, SECOND_OBJECT, SURFACE);

    assert!(was_sent_an_error(&mut rx));
    assert!(token.is_cancelled());
}

#[test]
fn destroying_the_surface_leaves_nothing_to_answer_for() {
    let (mut state, mut rx, _) = state_with_a_window();
    get_fractional_scale(&mut state, OBJECT, SURFACE);
    let _ = scales(&mut rx);

    state.destroy_surface(CLIENT, SURFACE);
    set_output_scale(&mut state, OutputId(1), 2.0);
    update_surface_outputs(&mut state);

    assert!(
        state.fractional_scales.is_empty(),
        "the object survives until the client destroys it, but it is about \
         nothing now and must not be answered for"
    );
    assert!(scales(&mut rx).is_empty());
}
