//! Tests for `wl_surface`: mapping a commit's damage rectangles into the
//! buffer pixels an upload can use, and the frame callbacks a commit owes.

use tokio::sync::mpsc::{Receiver, channel};
use tokio_util::sync::CancellationToken;
use tokio_way_backends::scene_graph::TextureRect;
use tokio_way_core::protocol::wire_utils::ArgWriter;
use tokio_way_core::protocol::wl_surface::{COMMIT, FRAME, committed_damage, handle};
use tokio_way_core::state::{CompositorState, ViewportState};
use tokio_way_sock::{WaylandEvent, WaylandRequest, WaylandRequestWithClientInfo};

const CLIENT: u32 = 1;
const SURFACE: u32 = 10;
const BUFFER: u32 = 11;
const VIEWPORT: u32 = 12;
const SIDE: i32 = 40;

fn rect(x: i32, y: i32, width: i32, height: i32) -> TextureRect {
    TextureRect {
        x,
        y,
        width,
        height,
    }
}

/// A surface with a `SIDE`-square buffer attached at the given buffer scale.
fn surface_with_buffer(buffer_scale: i32) -> CompositorState {
    let mut state = crate::tests::test_state();
    state.create_surface(CLIENT, SURFACE);
    state.register_buffer(CLIENT, BUFFER, 0, 0, SIDE, SIDE, SIDE * 4, 0);
    let surface = state.surfaces.get_mut(&(CLIENT, SURFACE)).unwrap();
    surface.buffer_id = Some(BUFFER);
    surface.buffer_scale = buffer_scale;
    state
}

fn damage_for(
    state: &CompositorState,
    surface: &[TextureRect],
    buffer: &[TextureRect],
) -> Vec<TextureRect> {
    committed_damage(state, (CLIENT, SURFACE), BUFFER, surface, buffer)
}

#[test]
fn a_commit_that_reports_no_damage_means_all_of_it() {
    let state = surface_with_buffer(1);
    assert!(damage_for(&state, &[], &[]).is_empty());
}

#[test]
fn buffer_damage_is_taken_as_is_and_clipped() {
    let state = surface_with_buffer(1);
    // Already in buffer pixels, so only the overhang needs trimming.
    assert_eq!(
        damage_for(&state, &[], &[rect(4, 4, 8, 8), rect(38, 38, 10, 10)]),
        vec![rect(4, 4, 8, 8), rect(38, 38, 2, 2)]
    );
}

#[test]
fn surface_damage_is_scaled_into_buffer_pixels() {
    // At scale 2 one surface pixel is two buffer pixels, so a 10-square of
    // surface damage covers a 20-square of buffer — plus a pixel of pad on
    // each side, because the mapping is not pixel-exact.
    let state = surface_with_buffer(2);
    assert_eq!(
        damage_for(&state, &[rect(5, 5, 10, 10)], &[]),
        vec![rect(9, 9, 22, 22)]
    );
}

#[test]
fn surface_damage_follows_the_viewport() {
    // A viewport showing the buffer's top-left quarter blown up to the full
    // surface: surface coordinates are then half as large in buffer terms.
    let mut state = surface_with_buffer(1);
    state.viewports.insert(
        (CLIENT, VIEWPORT),
        ViewportState {
            client_id: CLIENT,
            surface_id: SURFACE,
            source: Some((0.0, 0.0, 20.0, 20.0)),
            destination: Some((40, 40)),
            pending_source: None,
            pending_destination: None,
        },
    );
    state.surface_viewport.insert((CLIENT, SURFACE), VIEWPORT);

    assert_eq!(
        damage_for(&state, &[rect(10, 10, 20, 20)], &[]),
        vec![rect(4, 4, 12, 12)]
    );
}

#[test]
fn damage_that_lands_entirely_outside_the_buffer_widens_to_everything() {
    let state = surface_with_buffer(1);
    // Nothing survives clipping, so there is no rectangle to promise with.
    // Falling back to a full upload is wasteful but never wrong.
    assert!(damage_for(&state, &[], &[rect(100, 100, 5, 5)]).is_empty());
}

#[test]
fn damage_on_a_surface_with_no_mapping_widens_to_everything() {
    let mut state = surface_with_buffer(1);
    state
        .surfaces
        .get_mut(&(CLIENT, SURFACE))
        .unwrap()
        .buffer_id = None;
    assert!(damage_for(&state, &[rect(0, 0, 4, 4)], &[]).is_empty());
}

// -- frame callbacks ---------------------------------------------------------

/// Deliver a `wl_surface.frame` naming `callback_id`, then a `commit`.
fn request_frame(state: &mut CompositorState, callback_id: u32) {
    deliver_to_surface(state, FRAME, ArgWriter::new().u32(callback_id).build());
}

fn commit(state: &mut CompositorState) {
    deliver_to_surface(state, COMMIT, Vec::new());
}

fn deliver_to_surface(state: &mut CompositorState, op_code: u16, args: Vec<u8>) {
    handle(
        state,
        &WaylandRequestWithClientInfo {
            client_id: CLIENT,
            message: WaylandRequest {
                object_id: SURFACE,
                op_code,
                args,
            },
        },
    );
}

/// A surface belonging to a registered client, so `wl_surface.frame` can
/// register its callback object.
fn connected_surface() -> (CompositorState, Receiver<WaylandEvent>) {
    let mut state = surface_with_buffer(1);
    let (tx, rx) = channel(64);
    state.clients.create(CLIENT, tx, CancellationToken::new());
    state
        .clients
        .get(CLIENT)
        .unwrap()
        .register_client_object(SURFACE, tokio_way_core::protocol::ObjectType::WlSurface)
        .unwrap();
    (state, rx)
}

#[test]
fn every_frame_request_committed_together_is_kept() {
    // A GL client has two independent askers on one surface — its own paint
    // scheduler and the EGL swap throttle underneath it — so more than one
    // frame request between commits is ordinary, not pathological. Keeping
    // only the newest strands whichever asked first, and that client never
    // draws again.
    let (mut state, _rx) = connected_surface();
    request_frame(&mut state, 30);
    request_frame(&mut state, 31);
    commit(&mut state);

    assert_eq!(
        state.surfaces[&(CLIENT, SURFACE)].frame_callbacks,
        vec![30, 31],
        "both callbacks are owed a done, in the order they were committed"
    );
}

#[test]
fn a_callback_committed_earlier_is_still_owed_after_a_later_commit() {
    // The first commit's callback has not been answered yet — no frame has
    // gone out — so a second commit must not displace it.
    let (mut state, _rx) = connected_surface();
    request_frame(&mut state, 30);
    commit(&mut state);
    request_frame(&mut state, 31);
    commit(&mut state);

    assert_eq!(
        state.surfaces[&(CLIENT, SURFACE)].frame_callbacks,
        vec![30, 31]
    );
}

#[test]
fn a_frame_request_takes_effect_only_on_commit() {
    let (mut state, _rx) = connected_surface();
    request_frame(&mut state, 30);

    assert!(
        state.surfaces[&(CLIENT, SURFACE)]
            .frame_callbacks
            .is_empty(),
        "an uncommitted frame request is not owed a done yet"
    );
    assert_eq!(
        state.surfaces[&(CLIENT, SURFACE)].pending.frame_callbacks,
        vec![30]
    );
}
