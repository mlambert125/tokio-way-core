//! `wl_surface` protocol handler.
//!
//! A surface is a rectangular area of pixels that can be displayed. Clients
//! attach buffers, mark damage, request frame callbacks, and commit to make
//! changes visible. The compositor reads committed state during rendering.

use tracing::debug;

use tokio_way_backends::scene_graph::TextureRect;
use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::{ClientObjectId, CompositorState, PendingRegion, SurfacePending};
use super::ObjectType;
use super::wire_utils::{ArgReader, ArgWriter, build_message};

// Request opcodes
const DESTROY: u16 = 0;
const ATTACH: u16 = 1;
const DAMAGE: u16 = 2;
pub const FRAME: u16 = 3;
const SET_OPAQUE_REGION: u16 = 4;
const SET_INPUT_REGION: u16 = 5;
pub const COMMIT: u16 = 6;
const SET_BUFFER_TRANSFORM: u16 = 7;
const SET_BUFFER_SCALE: u16 = 8;
const DAMAGE_BUFFER: u16 = 9;
const OFFSET: u16 = 10;

// Event opcodes
const ENTER: u16 = 0;
const LEAVE: u16 = 1;

/// Send `wl_surface.enter` — this surface is now shown on the given output.
///
/// Clients need this to choose a buffer scale: `wl_output.scale` is per output,
/// so a surface cannot know what scale to render at until it knows which
/// outputs it is on. `output_object_id` is the client's own `wl_output` object,
/// not the compositor's internal id.
pub fn send_enter(
    state: &mut CompositorState,
    client_id: u32,
    surface_id: u32,
    output_object_id: u32,
) {
    debug!("wl_surface.enter: surface_id={surface_id} output={output_object_id}");
    if let Some(client) = state.clients.get(client_id) {
        let args = ArgWriter::new().u32(output_object_id).build();
        let _ = client.send(build_message(surface_id, ENTER, args));
    }
}

/// Send `wl_surface.leave` — this surface is no longer shown on the given output.
pub fn send_leave(
    state: &mut CompositorState,
    client_id: u32,
    surface_id: u32,
    output_object_id: u32,
) {
    debug!("wl_surface.leave: surface_id={surface_id} output={output_object_id}");
    if let Some(client) = state.clients.get(client_id) {
        let args = ArgWriter::new().u32(output_object_id).build();
        let _ = client.send(build_message(surface_id, LEAVE, args));
    }
}

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        DESTROY => handle_destroy(state, msg),
        ATTACH => handle_attach(state, msg),
        // The two damage requests differ in coordinate space, so they cannot
        // share a list: `damage` is surface-local, `damage_buffer` is in
        // buffer pixels, and only the latter is directly usable as an upload
        // region.
        DAMAGE => handle_damage(state, msg, DamageSpace::Surface),
        DAMAGE_BUFFER => handle_damage(state, msg, DamageSpace::Buffer),
        FRAME => handle_frame(state, msg),
        SET_INPUT_REGION => handle_set_input_region(state, msg),
        SET_BUFFER_SCALE => handle_set_buffer_scale(state, msg),
        OFFSET => handle_offset(state, msg),
        SET_OPAQUE_REGION => handle_set_opaque_region(state, msg),
        SET_BUFFER_TRANSFORM => handle_set_buffer_transform(state, msg),
        COMMIT => handle_commit(state, msg),
        _ => super::unknown_request(state, msg, "wl_surface"),
    }
}

fn handle_destroy(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let surface_id = msg.message.object_id;
    debug!("wl_surface.destroy: surface_id={}", surface_id);
    state.destroy_surface(msg.client_id, surface_id);
    state.dirty = true;
    if let Some(client) = state.clients.get(msg.client_id) {
        client.unregister(surface_id);
    }
}

fn handle_attach(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    // attach args: object buffer (id or 0 for null), int32 x, int32 y
    let (Some(buffer_id), Some(x), Some(y)) = (args.u32(), args.i32(), args.i32()) else {
        super::malformed_request(state, msg, "wl_surface");
        return;
    };

    let surface_id = msg.message.object_id;
    if let Some(surface) = state.surfaces.get_mut(&(msg.client_id, surface_id)) {
        surface.pending.buffer_attached = true;
        // buffer_id 0 means detach
        surface.pending.buffer_id = if buffer_id == 0 {
            None
        } else {
            Some(buffer_id)
        };
        accumulate_offset(&mut surface.pending.offset, x, y);
    }
}

fn handle_offset(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let (Some(x), Some(y)) = (args.i32(), args.i32()) else {
        super::malformed_request(state, msg, "wl_surface");
        return;
    };

    let surface_id = msg.message.object_id;
    if let Some(surface) = state.surfaces.get_mut(&(msg.client_id, surface_id)) {
        accumulate_offset(&mut surface.pending.offset, x, y);
    }
}

/// Add one offset onto the pending one, keeping it within bounds.
///
/// Offsets accumulate across attaches within a commit, so this adds rather than
/// replaces. Clamped for the same reason a subsurface position is: these are
/// raw `i32`s from the client and are later added to a cursor position, which
/// overflows if nothing bounds them.
fn accumulate_offset(pending: &mut (i32, i32), x: i32, y: i32) {
    *pending = super::super::state::clamp_surface_offset(
        pending.0.saturating_add(x),
        pending.1.saturating_add(y),
    );
}

/// Which coordinate space a damage rectangle arrived in.
#[derive(Debug, Clone, Copy)]
enum DamageSpace {
    /// `wl_surface.damage` — surface-local, so it has to be mapped through the
    /// viewport and buffer scale before it means anything to a texture.
    Surface,
    /// `wl_surface.damage_buffer` — already buffer pixels.
    Buffer,
}

fn handle_damage(
    state: &mut CompositorState,
    msg: &WaylandRequestWithClientInfo,
    space: DamageSpace,
) {
    let mut args = ArgReader::new(&msg.message.args);
    // damage args: int32 x, int32 y, int32 width, int32 height
    let (Some(x), Some(y), Some(width), Some(height)) =
        (args.i32(), args.i32(), args.i32(), args.i32())
    else {
        super::malformed_request(state, msg, "wl_surface");
        return;
    };

    let surface_id = msg.message.object_id;
    if let Some(surface) = state.surfaces.get_mut(&(msg.client_id, surface_id)) {
        let rect = TextureRect {
            x,
            y,
            width,
            height,
        };
        match space {
            DamageSpace::Surface => surface.pending.damage_surface.push(rect),
            DamageSpace::Buffer => surface.pending.damage_buffer.push(rect),
        }
    }
}

fn handle_frame(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };
    let mut args = ArgReader::new(&msg.message.args);
    // frame args: new_id callback
    let Some(callback_id) = args.new_id() else {
        super::malformed_request(state, msg, "wl_surface");
        return;
    };

    let surface_id = msg.message.object_id;

    if client
        .register(callback_id, ObjectType::WlCallback)
        .is_err()
    {
        return;
    }

    if let Some(surface) = state.surfaces.get_mut(&(msg.client_id, surface_id)) {
        // Appended, never replaced: every request is owed its own `done`, and
        // they are answered in the order they were committed.
        surface.pending.frame_callbacks.push(callback_id);
    }
}

/// `wl_surface.set_input_region` — restrict which parts of the surface accept
/// pointer input.
///
/// The region is *copied* rather than referenced: the protocol lets the client
/// destroy the `wl_region` immediately afterwards, and later changes to that
/// object must not affect the surface. Like most surface state it is
/// double-buffered, so it only takes effect on the next commit.
fn handle_set_input_region(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let Some(region) = decode_region(state, msg) else {
        return;
    };
    if let Some(surface) = state
        .surfaces
        .get_mut(&(msg.client_id, msg.message.object_id))
    {
        surface.pending.input_region = region;
    }
}

/// Record where the client promises its surface is fully opaque.
///
/// Purely an optimisation, and one the compositor is free to ignore — which is
/// why a region that cannot be resolved is dropped rather than treated as an
/// error. What it buys is skipping the alpha blend for surfaces that cover what
/// is behind them, which is most windows most of the time.
fn handle_set_opaque_region(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let Some(region) = decode_region(state, msg) else {
        return;
    };
    if let Some(surface) = state
        .surfaces
        .get_mut(&(msg.client_id, msg.message.object_id))
    {
        surface.pending.opaque_region = region;
    }
}

/// Record how the client has already transformed its buffer.
///
/// The compositor undoes it when drawing, and a quarter turn also exchanges the
/// surface's width and height — so this reaches hit testing and layout, not
/// only sampling.
fn handle_set_buffer_transform(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(value) = args.i32() else {
        super::malformed_request(state, msg, "wl_surface");
        return;
    };

    let Some(transform) = u32::try_from(value)
        .ok()
        .and_then(tokio_way_backends::scene_graph::BufferTransform::from_wire)
    else {
        if let Some(client) = state.clients.get(msg.client_id) {
            // wl_surface.error.invalid_transform = 0
            client.send_error(
                msg.message.object_id,
                0,
                "wl_surface.set_buffer_transform: not a transform the protocol defines",
            );
        }
        return;
    };

    if let Some(surface) = state
        .surfaces
        .get_mut(&(msg.client_id, msg.message.object_id))
    {
        surface.pending.buffer_transform = Some(transform);
    }
}

/// Read a nullable `wl_region` argument into a pending region.
///
/// A null region is the protocol default rather than an absence: for input it
/// means the whole surface, for opacity it means none of it, which is why the
/// two are distinguished at the point they are applied rather than here.
fn decode_region(
    state: &mut CompositorState,
    msg: &WaylandRequestWithClientInfo,
) -> Option<PendingRegion> {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(region_id) = args.u32() else {
        super::malformed_request(state, msg, "wl_surface");
        return None;
    };
    if region_id == 0 {
        return Some(PendingRegion::Infinite);
    }
    let rects = state
        .regions
        .get(&(msg.client_id, region_id))
        .map(|region| region.rects.clone())?;
    Some(PendingRegion::Rects(rects))
}

/// `wl_surface.set_buffer_scale` — how many buffer pixels map to one
/// surface-local coordinate.
///
/// A client on a scaled output submits a buffer this many times larger and sets
/// the scale, so the compositor knows the surface is still its logical size.
/// Double-buffered like the rest of surface state.
fn handle_set_buffer_scale(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let Some(scale) = ArgReader::new(&msg.message.args).i32() else {
        return;
    };
    let surface_id = msg.message.object_id;

    if scale < 1 {
        if let Some(client) = state.clients.get(msg.client_id) {
            // WL_SURFACE_ERROR_INVALID_SCALE = 0
            client.send_error(
                surface_id,
                0,
                "wl_surface.set_buffer_scale: scale must be >= 1",
            );
        }
        return;
    }

    debug!(
        "wl_surface.set_buffer_scale: surface_id={} scale={}",
        surface_id, scale
    );
    if let Some(surface) = state.surfaces.get_mut(&(msg.client_id, surface_id)) {
        surface.pending.buffer_scale = Some(scale);
    }
}

fn handle_commit(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let key = (msg.client_id, msg.message.object_id);
    let Some(surface) = state.surfaces.get_mut(&key) else {
        return;
    };
    let pending = std::mem::take(&mut surface.pending);

    // A layer surface's own double-buffered state applies here, with the rest
    // of the surface's: an anchor and a size that changed together have to
    // land together, or the panel is briefly the new size in the old place.
    // The first commit is also what asks the compositor for a size, which is
    // the protocol's handshake — a layer surface commits nothing, is
    // configured, and only then draws.
    if let Some(layer_key) = state.layer_surface_of(key) {
        if let Some(layer) = state.layer_surfaces.get_mut(&layer_key) {
            layer.current = layer.pending.clone();
        }
        super::zwlr_layer_surface::configure(state, layer_key);
        // Every other surface's geometry may have moved with it, because an
        // exclusive zone changes how much room is left for everyone.
        reconfigure_output_layers(state, layer_key);
        state.dirty = true;
    }

    // A window may not show content before it has agreed a configure. Checked
    // here rather than deeper in, because it is a property of *this commit*
    // being the one that first attaches a buffer, and because a refused commit
    // must not leave half its state applied — the pending state is already
    // taken above, so returning drops it whole.
    if pending.buffer_id.is_some()
        && !super::xdg_surface::check_configured_before_buffer(state, key)
    {
        return;
    }

    // A synchronised subsurface's commit does not reach the screen. It goes
    // into the cache, and the cache is applied when the parent's state is —
    // which is what makes a window and its subsurfaces update in one piece.
    // The cache accumulates rather than being replaced, because a client may
    // commit several times before its parent does and each of those commits
    // said something.
    if state.is_effectively_synced(key) {
        debug!(
            "wl_surface.commit: surface_id={} cached (synchronised)",
            key.1
        );
        let Some(surface) = state.surfaces.get_mut(&key) else {
            return;
        };
        surface
            .cached
            .get_or_insert_with(SurfacePending::default)
            .merge(pending);
        return;
    }

    apply_surface_state(state, key, pending);
}

/// Re-configure every layer surface sharing an output with this one.
///
/// An exclusive zone is a claim on space that everybody else's placement is
/// measured against, so one panel appearing moves every other. Cheap, because
/// `configure` sends nothing when a size has not changed.
fn reconfigure_output_layers(state: &mut CompositorState, changed: ClientObjectId) {
    let Some(output_id) = state
        .layer_surfaces
        .get(&changed)
        .and_then(|layer| layer.output)
    else {
        return;
    };
    let others: Vec<ClientObjectId> = state
        .layer_surfaces
        .iter()
        .filter(|(key, layer)| **key != changed && layer.output == Some(output_id))
        .map(|(&key, _)| key)
        .collect();
    for key in others {
        super::zwlr_layer_surface::configure(state, key);
    }
}

/// Apply one surface's committed state, and then the cached state of every
/// synchronised subsurface beneath it.
///
/// The second half is what synchronised mode means. The protocol puts it as
/// "the cached state is applied immediately after the parent surface's state
/// is applied", and it cascades: applying a subsurface's cache is itself the
/// moment its own synchronised children are applied. A client that builds a
/// tree of them gets the whole tree in one frame.
///
/// Walked with an explicit queue rather than by recursion. The tree is kept
/// acyclic — see [`CompositorState::is_ancestor`] — but this runs on every
/// commit of every client, and a queue costs nothing to read while taking the
/// depth of a client's surface tree off the stack entirely.
pub(crate) fn apply_surface_state(
    state: &mut CompositorState,
    key: ClientObjectId,
    pending: SurfacePending,
) {
    let mut queue = std::collections::VecDeque::from([(key, pending)]);
    while let Some((key, pending)) = queue.pop_front() {
        apply_one(state, key, pending);
        for child in state.synced_children(key) {
            // A child with nothing cached is still visited: its own children
            // may have something, and the moment the cache applies is decided
            // by the parent rather than by whether this surface happened to
            // commit.
            let cached = state
                .surfaces
                .get_mut(&child)
                .and_then(|surface| surface.cached.take())
                .unwrap_or_default();
            queue.push_back((child, cached));
        }
    }
}

/// Apply one surface's pending state to the state the compositor draws from.
fn apply_one(state: &mut CompositorState, key: ClientObjectId, mut pending: SurfacePending) {
    let (client_id, surface_id) = key;
    let mut committed_buffer = None;
    let damage_surface = std::mem::take(&mut pending.damage_surface);
    let damage_buffer = std::mem::take(&mut pending.damage_buffer);

    if let Some(surface) = state.surfaces.get_mut(&key) {
        // Apply pending buffer, releasing the old one if it changed
        if pending.buffer_attached {
            let new_buffer = pending.buffer_id;
            if let Some(old_buffer) = surface.buffer_id
                && new_buffer != Some(old_buffer)
            {
                state.buffers_pending_release.push((client_id, old_buffer));
            }
            surface.buffer_id = new_buffer;
        }

        // Move frame callbacks to committed state (fired on next render).
        // Appended in order, so a callback committed by an earlier commit is
        // still answered before one committed now.
        surface.frame_callbacks.extend(pending.frame_callbacks);

        // Move presentation feedbacks to committed state
        surface
            .presentation_feedbacks
            .extend(pending.presentation_feedbacks);

        // Apply pending buffer scale
        if let Some(scale) = pending.buffer_scale {
            surface.buffer_scale = scale;
        }

        // And the buffer transform, which is double-buffered for the same
        // reason: it changes the surface's size, and a size that changed
        // between an attach and its commit would tear.
        if let Some(transform) = pending.buffer_transform {
            surface.buffer_transform = transform;
        }

        // Apply the accumulated attach offset. Only the drag icon reads it —
        // see `Surface::offset` — but it is committed here like any other
        // double-buffered state so that it is correct if anything else comes
        // to need it.
        surface.offset = super::super::state::clamp_surface_offset(
            surface.offset.0.saturating_add(pending.offset.0),
            surface.offset.1.saturating_add(pending.offset.1),
        );

        // Apply pending input region
        match pending.input_region {
            PendingRegion::Unchanged => {}
            PendingRegion::Infinite => surface.input_region = None,
            PendingRegion::Rects(rects) => surface.input_region = Some(rects),
        }

        // And the opaque region, where the null case means the opposite: no
        // part of the surface is promised opaque.
        match pending.opaque_region {
            PendingRegion::Unchanged => {}
            PendingRegion::Infinite => surface.opaque_region = None,
            PendingRegion::Rects(rects) => surface.opaque_region = Some(rects),
        }

        committed_buffer = surface.buffer_id;

        state.dirty = true;
        debug!(
            "wl_surface.commit: surface_id={} buffer={:?}",
            surface_id, committed_buffer
        );
    }

    // A client's cursor surface lives outside the scene, so a commit into it
    // moves nothing the scene knows about; the cursor has to be rebuilt for the
    // new buffer to show. This is what makes an animated cursor animate.
    if state.cursor_role_surfaces.contains(&key) {
        state.cursor_dirty = true;
    }

    // Apply pending viewport state before reading damage: surface-local damage
    // is interpreted through the viewport this same commit installs.
    //
    // Deferred with the rest for a synchronised subsurface, which is what it
    // means for the viewport to be double-buffered surface state rather than
    // state of its own: a crop and the buffer it crops have to land together.
    if let Some(&vp_id) = state.surface_viewport.get(&key)
        && let Some(vp) = state.viewports.get_mut(&(client_id, vp_id))
    {
        if let Some(src) = vp.pending_source.take() {
            vp.source = Some(src);
        }
        if let Some(dst) = vp.pending_destination.take() {
            vp.destination = Some(dst);
        }
    }

    // A commit is the only point at which a client promises its buffer contents
    // are stable, and it may have redrawn into a buffer it never detached, so
    // every commit marks the attached buffer changed. What the damage adds is
    // how *much* of it changed.
    if let Some(buffer_id) = committed_buffer {
        let damage = committed_damage(state, key, buffer_id, &damage_surface, &damage_buffer);
        state.mark_buffer_damaged(client_id, buffer_id, &damage);
    }

    // A window mid-resize is repositioned against the size it just
    // committed, so the grab's anchored edges hold still however far this
    // client runs behind the pointer.
    state.apply_resize_anchor(key);
}

/// Convert a commit's damage into buffer-pixel rectangles.
///
/// An empty result means "assume the whole buffer changed", and every
/// uncertain case collapses to it: damage is a promise that everything
/// *outside* it is unchanged, so a rectangle we cannot place accurately is
/// worse than no rectangle at all.
pub fn committed_damage(
    state: &CompositorState,
    key: ClientObjectId,
    buffer_id: u32,
    damage_surface: &[TextureRect],
    damage_buffer: &[TextureRect],
) -> Vec<TextureRect> {
    if damage_surface.is_empty() && damage_buffer.is_empty() {
        return Vec::new();
    }
    let Some(buffer) = state.buffers.get(&(key.0, buffer_id)) else {
        return Vec::new();
    };
    let (width, height) = (buffer.width, buffer.height);

    let mut rects = Vec::with_capacity(damage_surface.len() + damage_buffer.len());
    rects.extend(
        damage_buffer
            .iter()
            .filter_map(|r| clamp_rect(*r, width, height)),
    );

    if !damage_surface.is_empty() {
        // Surface coordinates only mean something through the same mapping the
        // scene draws with, run backwards. Without it, nothing can be placed.
        let Some(mapping) = state.surface_buffer_mapping(key) else {
            return Vec::new();
        };
        let (src_x, src_y, src_w, src_h) = mapping.src;
        let scale_x = src_w / f64::from(mapping.dest_width);
        let scale_y = src_h / f64::from(mapping.dest_height);

        for rect in damage_surface {
            // Round outward and pad by a pixel. The mapping is not pixel-exact,
            // and uploading a little more than changed is always safe.
            let x0 = (src_x + f64::from(rect.x) * scale_x).floor() - 1.0;
            let y0 = (src_y + f64::from(rect.y) * scale_y).floor() - 1.0;
            let x1 = (src_x + f64::from(rect.x.saturating_add(rect.width)) * scale_x).ceil() + 1.0;
            let y1 = (src_y + f64::from(rect.y.saturating_add(rect.height)) * scale_y).ceil() + 1.0;
            let mapped = TextureRect {
                x: clamped_i32(x0),
                y: clamped_i32(y0),
                width: clamped_i32(x1 - x0),
                height: clamped_i32(y1 - y0),
            };
            if let Some(clamped) = clamp_rect(mapped, width, height) {
                rects.push(clamped);
            }
        }
    }

    rects
}

/// Clip a rectangle to a buffer, dropping it if nothing is left.
fn clamp_rect(rect: TextureRect, width: i32, height: i32) -> Option<TextureRect> {
    let x0 = rect.x.max(0);
    let y0 = rect.y.max(0);
    let x1 = rect.x.saturating_add(rect.width).min(width);
    let y1 = rect.y.saturating_add(rect.height).min(height);
    (x1 > x0 && y1 > y0).then_some(TextureRect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    })
}

/// Narrow a mapped coordinate to `i32`, saturating rather than wrapping.
///
/// These values come from client-supplied rectangles scaled by a client-chosen
/// viewport, so they can be any float at all; `as` saturates, which is what we
/// want, but the intent is worth naming.
#[allow(clippy::cast_possible_truncation)]
fn clamped_i32(value: f64) -> i32 {
    value as i32
}
