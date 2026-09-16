//! Tests for scene building: how a surface's buffer, viewport and scale
//! become a quad, which output a window is drawn on, and how damage is
//! carried to the backend.

use std::os::fd::IntoRawFd;
use std::sync::Arc;
use tokio::sync::mpsc::channel;
use tokio_util::sync::CancellationToken;
use tokio_way_backends::outputs::{
    OUTPUT_MODE_CURRENT, Output, OutputGeometry, OutputId, OutputMode, OutputSubpixel,
    OutputTransform, Scale,
};
use tokio_way_backends::scene_graph::{
    PixelFormat, Scene, SceneContent, SceneElement, TextureId, TextureImage, TextureRect,
    TextureSource,
};
use tokio_way_backends::shm::UploadPixels;
use tokio_way_core::scene::{SceneCache, build, build_cursor};
use tokio_way_core::state::{CompositorState, ViewportState};

const SURFACE_COLOUR: u32 = 0xffff_0000;
const BUFFER_SIDE: i32 = 40;
const OUTPUT_SIDE: i32 = 100;
const OUTPUT: OutputId = OutputId(1);
const CLIENT: u32 = 1;
const POOL_ID: u32 = 100;
const BUFFER_ID: u32 = 101;
const SURFACE_ID: u32 = 200;
const VIEWPORT_ID: u32 = 300;

fn test_output() -> Output {
    Output {
        id: OUTPUT,
        geometry: OutputGeometry {
            x: 0,
            y: 0,
            physical_width: OUTPUT_SIDE,
            physical_height: OUTPUT_SIDE,
            subpixel: OutputSubpixel::None,
            make: String::new(),
            model: String::new(),
            transform: OutputTransform::Normal,
        },
        modes: vec![OutputMode {
            flags: OUTPUT_MODE_CURRENT,
            width: OUTPUT_SIDE,
            height: OUTPUT_SIDE,
            refresh_mhz: 60000,
        }],
        scale: Scale::ONE,
        name: String::from("test"),
        description: String::from("test"),
    }
}

/// A compositor with one output and one client holding a solid-colour
/// `BUFFER_SIDE`-square buffer attached to a mapped surface at the origin.
fn test_state(buffer_scale: i32) -> CompositorState {
    let mut state = crate::tests::test_state();
    state.outputs.push(test_output());

    let (tx, _rx) = channel(64);
    state.clients.create(CLIENT, tx, CancellationToken::new());

    // A real memfd-backed pool, filled with a solid colour.
    let pixel_count = (BUFFER_SIDE * BUFFER_SIDE).unsigned_abs() as usize;
    let size = pixel_count * 4;
    let file = memfd_filled_with(size, SURFACE_COLOUR);
    state.register_shm_pool(
        CLIENT,
        POOL_ID,
        file.into_raw_fd(),
        size.try_into().unwrap(),
    );
    state.register_buffer(
        CLIENT,
        BUFFER_ID,
        POOL_ID,
        0,
        BUFFER_SIDE,
        BUFFER_SIDE,
        BUFFER_SIDE * 4,
        0,
    );

    state.create_surface(CLIENT, SURFACE_ID);
    let surface = state.surfaces.get_mut(&(CLIENT, SURFACE_ID)).unwrap();
    surface.buffer_id = Some(BUFFER_ID);
    surface.position = (0, 0);
    surface.buffer_scale = buffer_scale;
    // A window lives in a workspace, and only the workspace showing on an
    // output is drawn there.
    crate::tests::with_ws(&mut state, |w, s| w.workspaces.sync_outputs(&s.outputs));
    crate::tests::ws(&mut state).move_toplevel_to_output((CLIENT, SURFACE_ID), OUTPUT);

    state
}

fn memfd_filled_with(size: usize, colour: u32) -> std::fs::File {
    use std::io::Write;
    use std::os::fd::FromRawFd;
    let fd = unsafe { libc::memfd_create(c"scene-test".as_ptr().cast(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0, "memfd_create failed");
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let row: Vec<u8> = colour.to_ne_bytes().repeat(size / 4);
    file.write_all(&row).unwrap();
    file.flush().unwrap();
    file
}

/// Build a scene through a throwaway cache, for tests that only look at
/// one frame.
fn scene_of(state: &CompositorState) -> Scene {
    build(OUTPUT, 1, state, &mut SceneCache::new())
}

/// The texture an element samples, if it is a textured one.
fn texture_id(element: &SceneElement) -> Option<TextureId> {
    element.texture().map(|t| t.id)
}

/// The source crop of a textured element.
fn src_of(element: &SceneElement) -> (f64, f64, f64, f64) {
    match &element.content {
        SceneContent::Texture { src, .. } => *src,
        SceneContent::Color(_) | SceneContent::Group(_) => {
            panic!("expected a textured element, not a colour or group")
        }
    }
}

/// The one element drawn from the client's buffer. Found rather than indexed
/// so a drag icon or a bell flash in the scene does not throw the lookup off.
fn surface_element(scene: &Scene) -> &SceneElement {
    scene
        .elements
        .iter()
        .find(|e| texture_id(e) == Some(TextureId::Buffer(CLIENT, BUFFER_ID)))
        .expect("no element for the client buffer")
}

#[test]
fn unscaled_buffer_covers_its_full_size() {
    let state = test_state(1);
    let scene = scene_of(&state);
    let element = surface_element(&scene);
    assert_eq!(
        element.dst,
        (0.0, 0.0, f64::from(BUFFER_SIDE), f64::from(BUFFER_SIDE))
    );
    assert_eq!(
        src_of(element),
        (0.0, 0.0, f64::from(BUFFER_SIDE), f64::from(BUFFER_SIDE))
    );
}

#[test]
fn scaled_buffer_covers_its_logical_size() {
    // A scale-2 client submits a buffer twice as large in each axis, so the
    // same buffer must land on a quarter of the pixels — while still
    // sampling the whole of it.
    let state = test_state(2);
    let scene = scene_of(&state);
    let element = surface_element(&scene);
    assert_eq!(
        element.dst,
        (
            0.0,
            0.0,
            f64::from(BUFFER_SIDE / 2),
            f64::from(BUFFER_SIDE / 2)
        )
    );
    assert_eq!(
        src_of(element),
        (0.0, 0.0, f64::from(BUFFER_SIDE), f64::from(BUFFER_SIDE))
    );
}

#[test]
fn viewport_crops_the_source_and_sets_the_destination() {
    let mut state = test_state(2);
    state.viewports.insert(
        (CLIENT, VIEWPORT_ID),
        ViewportState {
            client_id: CLIENT,
            surface_id: SURFACE_ID,
            source: Some((4.0, 8.0, 16.0, 20.0)),
            destination: Some((70, 30)),
            pending_source: None,
            pending_destination: None,
        },
    );
    state
        .surface_viewport
        .insert((CLIENT, SURFACE_ID), VIEWPORT_ID);

    let scene = scene_of(&state);
    let element = surface_element(&scene);
    assert_eq!(src_of(element), (4.0, 8.0, 16.0, 20.0));
    // The viewport destination wins outright — buffer scale does not apply
    // on top of it.
    assert_eq!(element.dst, (0.0, 0.0, 70.0, 30.0));
}

#[test]
fn surface_position_offsets_the_destination() {
    let mut state = test_state(1);
    state
        .surfaces
        .get_mut(&(CLIENT, SURFACE_ID))
        .unwrap()
        .position = (12, -7);
    let scene = scene_of(&state);
    assert_eq!(
        surface_element(&scene).dst,
        (12.0, -7.0, f64::from(BUFFER_SIDE), f64::from(BUFFER_SIDE))
    );
}

fn rect(x: i32, y: i32, width: i32, height: i32) -> TextureRect {
    TextureRect {
        x,
        y,
        width,
        height,
    }
}

/// The buffer's texture, built through a cache that persists across calls.
/// What an image says about the copy it is a patch against, and what changed.
///
/// Every image these tests build is an upload — there is no client here to
/// hand over a GPU buffer — so a dma-buf means the test built the wrong thing.
fn upload_of(image: &TextureImage) -> (Option<u64>, &[TextureRect]) {
    match &image.source {
        TextureSource::Upload {
            previous_serial,
            damage,
            ..
        } => (*previous_serial, damage),
        TextureSource::Dmabuf { .. } => panic!("expected an uploaded image, not a dma-buf"),
    }
}

/// What changed in an image since the copy the backend holds.
fn damage_of(image: &TextureImage) -> &[TextureRect] {
    upload_of(image).1
}

/// The serial an image is a patch against, if it is one.
fn previous_serial_of(image: &TextureImage) -> Option<u64> {
    upload_of(image).0
}

fn texture_of(state: &CompositorState, cache: &mut SceneCache) -> Arc<TextureImage> {
    surface_element(&build(OUTPUT, 1, state, cache))
        .texture()
        .expect("the surface element is textured")
        .clone()
}

#[test]
fn an_idle_surface_keeps_its_serial() {
    let mut state = test_state(1);
    let mut cache = SceneCache::new();

    let first = texture_of(&state, &mut cache);
    let again = texture_of(&state, &mut cache);
    // A fresh handle each frame — there is no copy to reuse — but the same
    // serial, which is what tells the backend its texture is still good.
    assert_eq!(again.serial, first.serial);
    assert_eq!(previous_serial_of(&again), None);

    state.mark_buffer_damaged(CLIENT, BUFFER_ID, &[]);
    let after = texture_of(&state, &mut cache);
    assert!(after.serial > first.serial);
}

#[test]
fn damage_rides_along_as_a_patch_on_the_previous_copy() {
    let mut state = test_state(1);
    let mut cache = SceneCache::new();

    let first = texture_of(&state, &mut cache);
    // Nothing to patch against yet, so the whole image is the update.
    assert_eq!(previous_serial_of(&first), None);
    assert!(damage_of(&first).is_empty());
    state.clear_buffer_damage();

    state.mark_buffer_damaged(CLIENT, BUFFER_ID, &[rect(4, 5, 6, 7)]);
    let second = texture_of(&state, &mut cache);
    // Anchored to the copy the backend is holding, so it can patch rather
    // than re-upload.
    assert_eq!(previous_serial_of(&second), Some(first.serial));
    assert_eq!(damage_of(&second), [rect(4, 5, 6, 7)]);
}

#[test]
fn damage_accumulates_until_it_is_consumed() {
    let mut state = test_state(1);
    let mut cache = SceneCache::new();
    texture_of(&state, &mut cache);
    state.clear_buffer_damage();

    // Two commits landing between two scenes must both survive: the second
    // says nothing about what the first changed.
    state.mark_buffer_damaged(CLIENT, BUFFER_ID, &[rect(0, 0, 4, 4)]);
    state.mark_buffer_damaged(CLIENT, BUFFER_ID, &[rect(8, 8, 4, 4)]);
    let image = texture_of(&state, &mut cache);
    assert_eq!(damage_of(&image), [rect(0, 0, 4, 4), rect(8, 8, 4, 4)]);
}

#[test]
fn undescribed_damage_widens_to_the_whole_buffer_and_stays_there() {
    let mut state = test_state(1);
    let mut cache = SceneCache::new();
    texture_of(&state, &mut cache);
    state.clear_buffer_damage();

    state.mark_buffer_damaged(CLIENT, BUFFER_ID, &[rect(0, 0, 4, 4)]);
    // A change nobody could describe. No later rectangle can narrow the
    // window back down, because the undescribed change is still in it.
    state.mark_buffer_damaged(CLIENT, BUFFER_ID, &[]);
    state.mark_buffer_damaged(CLIENT, BUFFER_ID, &[rect(8, 8, 4, 4)]);
    assert!(damage_of(&texture_of(&state, &mut cache)).is_empty());
}

#[test]
fn a_resized_buffer_cannot_be_patched() {
    let mut state = test_state(1);
    let mut cache = SceneCache::new();
    texture_of(&state, &mut cache);
    state.clear_buffer_damage();

    // Same buffer id, different shape: the rectangles no longer refer to
    // anything the backend holds, even where they overlap.
    state.register_buffer(
        CLIENT,
        BUFFER_ID,
        POOL_ID,
        0,
        BUFFER_SIDE / 2,
        BUFFER_SIDE / 2,
        (BUFFER_SIDE / 2) * 4,
        0,
    );
    state.mark_buffer_damaged(CLIENT, BUFFER_ID, &[rect(0, 0, 4, 4)]);
    assert!(damage_of(&texture_of(&state, &mut cache)).is_empty());
}

#[test]
fn dead_buffers_are_reaped_from_the_cache() {
    let mut state = test_state(1);
    let mut cache = SceneCache::new();
    texture_of(&state, &mut cache);

    state.destroy_buffer(CLIENT, BUFFER_ID);
    cache.gc(&state);
    assert!(cache.is_empty());
}

#[test]
fn buffer_pixels_are_borrowed_from_the_client_mapping() {
    let state = test_state(1);
    let scene = scene_of(&state);
    let texture = surface_element(&scene)
        .texture()
        .expect("the surface element is textured")
        .clone();

    assert_eq!(texture.format, PixelFormat::Argb8888);
    assert_eq!(texture.width, BUFFER_SIDE);
    assert_eq!(texture.height, BUFFER_SIDE);
    // Pointing at the client's pool, not at a copy of it.
    assert!(matches!(
        texture.source,
        TextureSource::Upload {
            pixels: UploadPixels::Mapped { .. },
            ..
        }
    ));

    // SAFETY: the image is alive, so nothing has been released.
    let bytes = unsafe { texture.bytes() }.expect("image does not fit its mapping");
    assert_eq!(
        bytes.len(),
        (BUFFER_SIDE * BUFFER_SIDE).unsigned_abs() as usize * 4
    );
    assert_eq!(bytes[..4], SURFACE_COLOUR.to_le_bytes());
}

#[test]
fn a_buffer_being_drawn_is_not_released_until_the_frame_goes() {
    let state = test_state(1);
    let key = (CLIENT, BUFFER_ID);
    // Nothing has borrowed it yet.
    assert!(!state.buffer_is_being_read(key));

    let scene = scene_of(&state);
    // The scene holds the only borrow, so the buffer is in use.
    assert!(state.buffer_is_being_read(key));

    drop(scene);
    // Frame gone, buffer free — this is what gates `wl_buffer.release`.
    assert!(!state.buffer_is_being_read(key));
}

#[test]
fn buffer_larger_than_its_pool_is_dropped() {
    let mut state = test_state(1);
    // Claim a buffer twice as tall as the pool can hold. Reading it would
    // run off the end of the mapping, so it must contribute no element.
    state.register_buffer(
        CLIENT,
        BUFFER_ID,
        POOL_ID,
        0,
        BUFFER_SIDE,
        BUFFER_SIDE * 2,
        BUFFER_SIDE * 4,
        0,
    );

    let scene = scene_of(&state);
    assert!(
        !scene
            .elements
            .iter()
            .any(|e| texture_id(e) == Some(TextureId::Buffer(CLIENT, BUFFER_ID)))
    );
}

/// The cursor, built through a throwaway cache. It rides in the frame beside
/// the scenes now, not inside one, so it is inspected on its own.
fn cursor_of(state: &CompositorState) -> tokio_way_backends::scene_graph::Cursor {
    build_cursor(state, &mut SceneCache::new())
}

#[test]
fn cursor_is_drawn_when_no_client_has_set_one() {
    let mut state = test_state(1);
    state.cursor_x = 30.0;
    state.cursor_y = 40.0;

    let cursor = cursor_of(&state);
    assert_eq!(cursor.output, Some(OUTPUT));
    let element = cursor
        .elements
        .iter()
        .find(|e| texture_id(e) == Some(TextureId::FallbackCursor))
        .expect("no cursor element");
    assert_eq!((element.dst.0, element.dst.1), (30.0, 40.0));
    // The cursor is no longer a scene element — pointer motion must not touch
    // the scene.
    assert!(
        !scene_of(&state)
            .elements
            .iter()
            .any(|e| texture_id(e) == Some(TextureId::FallbackCursor))
    );
}

#[test]
fn client_can_hide_the_cursor() {
    let mut state = test_state(1);
    state.pointer_surface = Some((CLIENT, SURFACE_ID));
    state.cursor_surfaces.insert(CLIENT, None);

    // The pointer is over an output, but the client asked for no cursor: the
    // output is known, and there is nothing to draw.
    let cursor = cursor_of(&state);
    assert_eq!(cursor.output, Some(OUTPUT));
    assert!(cursor.elements.is_empty());
}

const OUTPUT_B: OutputId = OutputId(2);

/// A second output placed to the right of the first.
fn add_second_output(state: &mut CompositorState) {
    let mut output = test_output();
    output.id = OUTPUT_B;
    output.geometry.x = OUTPUT_SIDE;
    state.outputs.push(output);
    // The new output arrives with a workspace of its own, empty until a
    // window is moved onto it.
    crate::tests::with_ws(state, |w, s| w.workspaces.sync_outputs(&s.outputs));
}

#[test]
fn a_window_is_drawn_only_on_the_output_it_belongs_to() {
    let mut state = test_state(1);
    add_second_output(&mut state);
    let mut cache = SceneCache::new();

    // Belongs to the first output, so the second one draws nothing of it.
    assert!(
        build(OUTPUT, 1, &state, &mut cache)
            .elements
            .iter()
            .any(|e| texture_id(e) == Some(TextureId::Buffer(CLIENT, BUFFER_ID)))
    );
    assert!(
        !build(OUTPUT_B, 1, &state, &mut cache)
            .elements
            .iter()
            .any(|e| texture_id(e) == Some(TextureId::Buffer(CLIENT, BUFFER_ID)))
    );
}

#[test]
fn a_window_is_drawn_in_its_own_outputs_coordinates() {
    let mut state = test_state(1);
    add_second_output(&mut state);

    // Move it onto the second output, whose origin is OUTPUT_SIDE across.
    crate::tests::ws(&mut state).move_toplevel_to_output((CLIENT, SURFACE_ID), OUTPUT_B);
    let surface = state.surfaces.get_mut(&(CLIENT, SURFACE_ID)).unwrap();
    surface.position = (OUTPUT_SIDE + 10, 20);

    let scene = build(OUTPUT_B, 1, &state, &mut SceneCache::new());
    // Global x of OUTPUT_SIDE + 10 is x = 10 on that output's own surface.
    assert_eq!(
        surface_element(&scene).dst,
        (10.0, 20.0, f64::from(BUFFER_SIDE), f64::from(BUFFER_SIDE))
    );
}

#[test]
fn the_cursor_is_drawn_only_on_the_output_under_it() {
    let mut state = test_state(1);
    add_second_output(&mut state);
    // Pointer sits on the second output.
    state.cursor_x = f64::from(OUTPUT_SIDE) + 30.0;
    state.cursor_y = 40.0;

    let cursor = cursor_of(&state);
    // The cursor belongs to the output under the pointer, and its quad is in
    // that output's own coordinates.
    assert_eq!(cursor.output, Some(OUTPUT_B));
    let element = cursor.elements.first().expect("a cursor element");
    assert_eq!((element.dst.0, element.dst.1), (30.0, 40.0));
}

#[test]
fn a_window_dragged_past_its_output_edge_stays_put() {
    // Nothing keeps a window wholly inside its output any more: the part
    // hanging off the edge is simply not drawn, rather than the window being
    // pulled back.
    let mut state = test_state(1);
    add_second_output(&mut state);

    let position = (OUTPUT_SIDE - 5, 0);
    state
        .surfaces
        .get_mut(&(CLIENT, SURFACE_ID))
        .unwrap()
        .position = position;

    assert!(
        !crate::tests::with_ws(&mut state, crate::tests::TestShell::rehome_toplevels),
        "nothing should have moved"
    );
    assert_eq!(state.surfaces[&(CLIENT, SURFACE_ID)].position, position);
}

#[test]
fn a_window_whose_output_vanished_is_rehomed() {
    let mut state = test_state(1);
    add_second_output(&mut state);
    // The output the window was on is unplugged, taking its workspace — and so
    // the window's home — with it.
    state.outputs.retain(|o| o.id != OUTPUT);

    assert!(crate::tests::with_ws(
        &mut state,
        crate::tests::TestShell::rehome_toplevels
    ));
    let output = state.surface_output((CLIENT, SURFACE_ID));
    assert_eq!(
        output,
        Some(OUTPUT_B),
        "should have been re-homed onto the output that is left"
    );
}

// -- The drag icon -----------------------------------------------------------

const ICON_ID: u32 = 400;
const ICON_BUFFER_ID: u32 = 401;

/// Give a client a second surface with a buffer, and make it the icon of a
/// drag in progress.
fn start_drag_with_an_icon(state: &mut CompositorState, offset: (i32, i32)) {
    let pixel_count = (BUFFER_SIDE * BUFFER_SIDE).unsigned_abs() as usize;
    let size = pixel_count * 4;
    let file = memfd_filled_with(size, 0xff00_ff00);
    state.register_shm_pool(
        CLIENT,
        POOL_ID + 1,
        file.into_raw_fd(),
        size.try_into().unwrap(),
    );
    state.register_buffer(
        CLIENT,
        ICON_BUFFER_ID,
        POOL_ID + 1,
        0,
        BUFFER_SIDE,
        BUFFER_SIDE,
        BUFFER_SIDE * 4,
        0,
    );
    state.create_surface(CLIENT, ICON_ID);
    let icon = state.surfaces.get_mut(&(CLIENT, ICON_ID)).unwrap();
    icon.buffer_id = Some(ICON_BUFFER_ID);
    icon.offset = offset;

    state.dnd_icon_surfaces.insert((CLIENT, ICON_ID));
    state.start_drag(None, CLIENT, (CLIENT, SURFACE_ID), Some((CLIENT, ICON_ID)));
}

#[test]
fn no_drag_means_no_icon() {
    let state = test_state(1);
    // Just the window; the cursor rides in the frame, not the scene.
    assert_eq!(scene_of(&state).elements.len(), 1, "only the window");
}

#[test]
fn the_drag_icon_is_drawn_above_the_windows() {
    let mut state = test_state(1);
    state.cursor_x = 50.0;
    state.cursor_y = 50.0;
    start_drag_with_an_icon(&mut state, (0, 0));

    let scene = scene_of(&state);
    assert_eq!(scene.elements.len(), 2, "window, then icon");
    // Order is z-order: the icon is what is being carried, so it goes over
    // every window. The cursor is composited above it from the frame.
    assert_eq!(
        (scene.elements[1].dst.0, scene.elements[1].dst.1),
        (50.0, 50.0)
    );
    assert_eq!(
        (scene.elements[1].dst.2, scene.elements[1].dst.3),
        (f64::from(BUFFER_SIDE), f64::from(BUFFER_SIDE))
    );
}

#[test]
fn the_drag_icon_follows_the_attach_offset() {
    // A toolkit centres its icon under the pointer by attaching at a negative
    // dx and dy, and that offset is the only means it has to position it.
    let mut state = test_state(1);
    state.cursor_x = 50.0;
    state.cursor_y = 50.0;
    start_drag_with_an_icon(&mut state, (-20, -20));

    let scene = scene_of(&state);
    assert_eq!(
        (scene.elements[1].dst.0, scene.elements[1].dst.1),
        (30.0, 30.0)
    );
}

#[test]
fn ending_the_drag_takes_the_icon_out_of_the_scene() {
    let mut state = test_state(1);
    state.cursor_x = 50.0;
    state.cursor_y = 50.0;
    start_drag_with_an_icon(&mut state, (0, 0));
    assert_eq!(scene_of(&state).elements.len(), 2, "window and icon");

    state.cancel_drag();
    assert_eq!(scene_of(&state).elements.len(), 1, "just the window");
}

#[test]
fn the_drag_icon_is_only_drawn_on_the_output_holding_the_pointer() {
    let mut state = test_state(1);
    // The pointer is off this output entirely.
    state.cursor_x = f64::from(OUTPUT_SIDE) + 50.0;
    state.cursor_y = f64::from(OUTPUT_SIDE) + 50.0;
    start_drag_with_an_icon(&mut state, (0, 0));

    // Just the window: the drag icon does not belong on an output the pointer
    // is not over.
    assert_eq!(scene_of(&state).elements.len(), 1);
}

// -- Scene-level damage ------------------------------------------------------

use tokio_way_backends::scene_graph::TextureRect as Rect;
use tokio_way_core::scene::output_damage;

#[test]
fn an_unchanged_scene_has_no_damage() {
    let state = test_state(1);
    let a = scene_of(&state);
    let b = scene_of(&state);
    assert!(
        output_damage(&a, &b).is_empty(),
        "nothing moved, so nothing is damaged"
    );
}

#[test]
fn redrawing_a_buffer_damages_that_window() {
    let mut state = test_state(1);
    let before = scene_of(&state);

    // A commit into the same buffer bumps its content serial.
    state.mark_buffer_damaged(CLIENT, BUFFER_ID, &[]);
    let after = scene_of(&state);

    assert_eq!(
        output_damage(&before, &after),
        vec![Rect {
            x: 0,
            y: 0,
            width: BUFFER_SIDE,
            height: BUFFER_SIDE,
        }]
    );
}

#[test]
fn moving_a_window_damages_where_it_left_and_where_it_landed() {
    let mut state = test_state(1);
    let before = scene_of(&state); // window at the origin

    // Slide the window down and to the right; nothing else changes.
    state
        .surfaces
        .get_mut(&(CLIENT, SURFACE_ID))
        .unwrap()
        .position = (50, 60);
    let after = scene_of(&state);

    let damage = output_damage(&before, &after);
    // Two rectangles: the vacated origin and the new position, both the size of
    // the window.
    assert_eq!(damage.len(), 2);
    assert!(damage.contains(&Rect {
        x: 0,
        y: 0,
        width: BUFFER_SIDE,
        height: BUFFER_SIDE,
    }));
    assert!(damage.contains(&Rect {
        x: 50,
        y: 60,
        width: BUFFER_SIDE,
        height: BUFFER_SIDE,
    }));
}

// --- Named cursor shapes ------------------------------------------------------

use tokio_way_core::protocol::wp_cursor_shape_device::names_for;
use tokio_way_core::state::DefaultCursor;

/// A 2x2 cursor image, standing in for one the theme would have provided.
fn stub_cursor() -> DefaultCursor {
    DefaultCursor {
        pixels: vec![0xffff_ffff; 4],
        width: 2,
        height: 2,
        hotspot_x: 1,
        hotspot_y: 1,
    }
}

#[test]
fn a_named_shape_is_drawn_at_its_own_hotspot() {
    let mut state = test_state(1);
    state.cursor_x = 30.0;
    state.cursor_y = 40.0;
    state.pointer_surface = Some((CLIENT, SURFACE_ID));
    // `text`, with a hotspot in the middle of a 2x2 image.
    let names = names_for(9).unwrap();
    state.cursor_shape_images.insert(names, stub_cursor());
    state.cursor_shapes.insert(CLIENT, names);

    let cursor = cursor_of(&state);

    let element = cursor
        .elements
        .iter()
        .find(|e| texture_id(e) == Some(TextureId::DefaultCursor))
        .expect("the shape should be drawn through the themed cursor slot");
    // Positioned by the shape's hotspot, not the default cursor's.
    assert_eq!((element.dst.0, element.dst.1), (29.0, 39.0));
    assert_eq!((element.dst.2, element.dst.3), (2.0, 2.0));
}

#[test]
fn a_shape_with_no_image_falls_back_to_the_ordinary_pointer() {
    let mut state = test_state(1);
    state.pointer_surface = Some((CLIENT, SURFACE_ID));
    // Named but never loaded, which is the theme changing under a running
    // compositor: `set_shape` only records shapes it managed to load.
    state.cursor_shapes.insert(CLIENT, names_for(9).unwrap());

    let cursor = cursor_of(&state);

    // No theme is loaded in a test, so the ordinary pointer here is the built-in
    // one. What matters is that something is drawn rather than nothing.
    assert!(
        cursor
            .elements
            .iter()
            .any(|e| texture_id(e) == Some(TextureId::FallbackCursor)),
        "a missing image must not leave the pointer invisible"
    );
}

#[test]
fn changing_shape_rebuilds_the_cursor_texture() {
    let mut state = test_state(1);
    state.pointer_surface = Some((CLIENT, SURFACE_ID));
    let text = names_for(9).unwrap();
    let grab = names_for(16).unwrap();
    state.cursor_shape_images.insert(text, stub_cursor());
    state.cursor_shape_images.insert(
        grab,
        DefaultCursor {
            width: 4,
            height: 4,
            pixels: vec![0xff00_00ff; 16],
            ..stub_cursor()
        },
    );

    // One cache across both frames, as the compositor loop has.
    let mut cache = SceneCache::new();
    state.cursor_shapes.insert(CLIENT, text);
    let first = build_cursor(&state, &mut cache);
    state.cursor_shapes.insert(CLIENT, grab);
    let second = build_cursor(&state, &mut cache);

    // One texture id holds whichever shape is current, so the id repeats and the
    // serial must not: that is what tells the backend to upload again.
    let sizes = |cursor: &tokio_way_backends::scene_graph::Cursor| {
        cursor
            .elements
            .first()
            .map(|e| (e.dst.2, e.dst.3))
            .expect("a cursor element")
    };
    assert_eq!(sizes(&first), (2.0, 2.0));
    assert_eq!(sizes(&second), (4.0, 4.0), "the new shape's image is used");
}

// -- Interactive-resize churn: the window must never miss a frame -----------

/// Drive the commit pattern a toolkit produces during an interactive resize —
/// a fresh pool and buffer per size, the old pair destroyed as soon as the
/// new one is committed — composing a scene at every step. The window must
/// appear in every one of them: a single missed frame is the "nothing
/// frames" flicker on screen.
#[test]
fn a_window_is_drawn_in_every_frame_of_a_resize_churn() {
    use std::os::fd::IntoRawFd;
    let mut state = test_state(1);
    let mut cache = SceneCache::new();
    state.cursor_x = 30.0;
    state.cursor_y = 30.0;
    state.start_resize_grab(
        (CLIENT, SURFACE_ID),
        tokio_way_core::state::ResizeEdges(tokio_way_core::state::ResizeEdges::LEFT),
    );

    let mut old = (POOL_ID, BUFFER_ID);
    for step in 1..12i32 {
        // The pointer moves; the compositor asks for a new size.
        tokio_way_core::input::update_grab(&mut state, 30.0 + f64::from(step) * 2.0, 30.0);

        // A frame composes before the client catches up — the old buffer is
        // still current, so the window must still be there.
        let scene = build(OUTPUT, u64::try_from(step).unwrap() * 2, &state, &mut cache);
        assert!(
            scene.elements.iter().any(|e| e.texture().is_some()),
            "window missing from the scene while the client lags (step {step})",
        );

        // The client answers: new pool, new buffer at the new size,
        // attach + commit...
        let width = BUFFER_SIDE - step * 2;
        let (pool_id, buffer_id) = (
            POOL_ID + step.unsigned_abs() * 2,
            BUFFER_ID + step.unsigned_abs() * 2,
        );
        let size = (width * width * 4).unsigned_abs() as usize;
        let file = memfd_filled_with(size, SURFACE_COLOUR);
        state.register_shm_pool(
            CLIENT,
            pool_id,
            file.into_raw_fd(),
            size.try_into().unwrap(),
        );
        state.register_buffer(CLIENT, buffer_id, pool_id, 0, width, width, width * 4, 0);
        let surface = state.surfaces.get_mut(&(CLIENT, SURFACE_ID)).unwrap();
        let replaced = surface.buffer_id.replace(buffer_id);
        if let Some(replaced) = replaced {
            state.buffers_pending_release.push((CLIENT, replaced));
        }
        state.apply_resize_anchor((CLIENT, SURFACE_ID));

        // ...and destroys the pair it no longer needs, exactly as fast as a
        // real toolkit does.
        state.destroy_shm_pool(CLIENT, old.0);
        state.destroy_buffer(CLIENT, old.1);
        old = (pool_id, buffer_id);

        // Housekeeping runs its release bookkeeping...
        tokio_way_core::input::start_buffer_releases(&mut state);
        tokio_way_core::input::finish_buffer_releases(&mut state);

        // ...and the next frame composes from the new buffer.
        let scene = build(
            OUTPUT,
            u64::try_from(step).unwrap() * 2 + 1,
            &state,
            &mut cache,
        );
        let element = scene
            .elements
            .iter()
            .find(|e| e.texture().is_some())
            .unwrap_or_else(|| panic!("window missing right after its commit (step {step})"));
        assert!(
            (element.dst.2 - f64::from(width)).abs() < f64::EPSILON,
            "window drawn at the wrong size (step {step})",
        );
    }
}

#[test]
fn a_buffer_destroyed_while_attached_keeps_its_contents_on_screen() {
    use std::os::fd::IntoRawFd;
    // Toolkits destroy an obsolete buffer mid-resize while it is still the
    // surface's committed one, a beat before the replacement commits. The
    // contents are "undefined" for that gap; drawing nothing flickered a
    // blank frame on every resize step, so the useful reading is: what was
    // there last.
    let mut state = test_state(1);
    let mut cache = SceneCache::new();

    // Composed once, so the cache has seen the window.
    let before = scene_of_cached(&state, &mut cache);
    let shown = texture_id(surface_element(&before)).expect("a textured window");

    // The client destroys the committed buffer; the window must keep its
    // contents, not vanish.
    state.destroy_buffer(CLIENT, BUFFER_ID);
    let during = build(OUTPUT, 2, &state, &mut cache);
    let element = during
        .elements
        .iter()
        .find(|e| e.texture().is_some())
        .expect("the window must survive its buffer's destruction");
    assert_eq!(texture_id(element), Some(shown), "with the same contents");

    // The replacement commits at a new size, and the window follows it.
    let width = BUFFER_SIDE + 10;
    let size = (width * width * 4).unsigned_abs() as usize;
    let file = memfd_filled_with(size, SURFACE_COLOUR);
    state.register_shm_pool(
        CLIENT,
        POOL_ID + 1,
        file.into_raw_fd(),
        size.try_into().unwrap(),
    );
    state.register_buffer(
        CLIENT,
        BUFFER_ID + 1,
        POOL_ID + 1,
        0,
        width,
        width,
        width * 4,
        0,
    );
    state
        .surfaces
        .get_mut(&(CLIENT, SURFACE_ID))
        .unwrap()
        .buffer_id = Some(BUFFER_ID + 1);
    let after = build(OUTPUT, 3, &state, &mut cache);
    let element = after
        .elements
        .iter()
        .find(|e| e.texture().is_some())
        .unwrap();
    assert!((element.dst.2 - f64::from(width)).abs() < f64::EPSILON);
}

#[test]
fn an_unmapped_surface_does_not_linger_through_retained_contents() {
    // Retention is for a buffer that stopped existing, not for a client that
    // chose to show nothing: attach(null) + commit must still unmap.
    let mut state = test_state(1);
    let mut cache = SceneCache::new();
    let before = scene_of_cached(&state, &mut cache);
    assert!(surface_element(&before).texture().is_some());

    state
        .surfaces
        .get_mut(&(CLIENT, SURFACE_ID))
        .unwrap()
        .buffer_id = None;
    cache.gc(&state);
    let after = build(OUTPUT, 2, &state, &mut cache);
    assert!(
        after.elements.iter().all(|e| e.texture().is_none()),
        "an unmapped window draws nothing, retained or not",
    );
}

/// Like `scene_of`, but through a caller-held cache, for tests that need the
/// cache to remember the frame.
fn scene_of_cached(state: &CompositorState, cache: &mut SceneCache) -> Scene {
    build(OUTPUT, 1, state, cache)
}

#[test]
fn retained_contents_survive_housekeeping_that_ran_before_the_gap() {
    // The sequence the first version of retention lost: compose, an idle
    // housekeeping tick collects the cache while the buffer is still alive,
    // and only then does the client destroy it. The entry must still be
    // there when the gap opens — collecting it early is what turned every
    // resize step into a blank frame.
    let mut state = test_state(1);
    let mut cache = SceneCache::new();
    let before = scene_of_cached(&state, &mut cache);
    assert!(surface_element(&before).texture().is_some());

    cache.gc(&state);
    state.destroy_buffer(CLIENT, BUFFER_ID);
    let during = build(OUTPUT, 2, &state, &mut cache);
    assert!(
        during.elements.iter().any(|e| e.texture().is_some()),
        "housekeeping before the gap must not cost the window its fallback",
    );
}

#[test]
fn a_buffer_destroyed_before_it_was_ever_composed_falls_back_further() {
    use std::os::fd::IntoRawFd;
    // Under resize pressure a client can attach a buffer and destroy it
    // before a single compose sees it. The only contents left for that gap
    // are the last ones actually composed — from the buffer before.
    let mut state = test_state(1);
    let mut cache = SceneCache::new();
    let before = scene_of_cached(&state, &mut cache);
    let shown = texture_id(surface_element(&before)).expect("a textured window");

    // Attach a replacement the compositor never composes...
    let width = BUFFER_SIDE + 6;
    let size = (width * width * 4).unsigned_abs() as usize;
    let file = memfd_filled_with(size, SURFACE_COLOUR);
    state.register_shm_pool(
        CLIENT,
        POOL_ID + 1,
        file.into_raw_fd(),
        size.try_into().unwrap(),
    );
    state.register_buffer(
        CLIENT,
        BUFFER_ID + 1,
        POOL_ID + 1,
        0,
        width,
        width,
        width * 4,
        0,
    );
    state
        .surfaces
        .get_mut(&(CLIENT, SURFACE_ID))
        .unwrap()
        .buffer_id = Some(BUFFER_ID + 1);
    // ...housekeeping collects in between...
    cache.gc(&state);
    // ...and the client destroys it before the compositor drew it once.
    state.destroy_buffer(CLIENT, BUFFER_ID + 1);

    let during = build(OUTPUT, 2, &state, &mut cache);
    let element = during
        .elements
        .iter()
        .find(|e| e.texture().is_some())
        .expect("the window survives on the last contents that ever composed");
    assert_eq!(texture_id(element), Some(shown));
}
