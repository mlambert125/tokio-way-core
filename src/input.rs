//! Input delivery and the state-adjacent mechanism the loop leans on.
//!
//! Everything here behaves identically whatever the window-management policy:
//! pointer, touch and scroll delivery with correct enter/leave and serial
//! discipline, interactive move/resize grabs, drag-and-drop, hit testing,
//! frame callbacks and presentation feedback, buffer release, and the focus
//! *mechanics* (who is told about a focus change — deciding who gets focus is
//! the shell's job, which calls [`switch_focus`] and [`clear_focus`] to make
//! it so). Where a question is policy — what is visible, in what order — it
//! is asked through `state.shell`.

use crate::protocol::wire_utils::{ArgWriter, build_message, f64_to_i32};
use crate::protocol::{
    cancel_source, wl_data_device, wl_data_offer, wl_data_source, wl_keyboard, wl_pointer,
    wl_surface, wl_touch, wp_fractional_scale, wp_presentation_feedback, xdg_popup, xdg_surface,
    xdg_toplevel, zwp_primary_selection_device,
};
use crate::state::{
    ClientObjectId, CompositorState, GrabKind, OfferKind, ResizeEdges, region_contains,
};
use crate::{MIN_WINDOW_HEIGHT, MIN_WINDOW_WIDTH, SCROLL_STEP, layer, state};
use std::collections::HashSet;
use tokio_way_backends::input::ScrollSource;
use tokio_way_backends::monotonic_timestamp::MonotonicTimeStamp;
use tokio_way_backends::outputs::{OutputId, output_contains};
use tracing::debug;

/// Whose frame callbacks a presentation settles.
#[derive(Debug, Clone, Copy)]
pub(crate) enum FrameTarget {
    /// The surfaces shown on one output, because that output presented.
    Output(OutputId),
    /// The surfaces no output is showing.
    ///
    /// A client waiting on `wl_surface.frame` for one of these would otherwise
    /// wait forever, and it is not always a window the user has hidden: a
    /// client's first commit typically carries no buffer and a frame request,
    /// and it will not draw anything until that callback comes back.
    Offscreen,
}

/// What a hit test found under a point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HitResult {
    /// The toplevel surface key — the thing stacking and keyboard focus act on.
    pub toplevel: ClientObjectId,
    /// The specific surface under the pointer (possibly a subsurface) — the
    /// thing pointer events are delivered to.
    pub surface: ClientObjectId,
    /// Global x position of the specific surface, for computing local coordinates.
    pub surface_x: i32,
    /// Global y position of the specific surface, for computing local coordinates.
    pub surface_y: i32,
}

/// Work out what the pointer is now over, and tell the client about it.
///
/// Reads the position from state rather than taking one, because by this point
/// it has been constrained onto an output and the raw value the backend sent is
/// no longer the truth.
pub fn deliver_pointer_motion(state: &mut CompositorState, time_ms: u32) {
    // The pointer moved, so the cursor must be redrawn — but that alone is not
    // a scene change, which is the whole point of keeping the cursor out of the
    // scene. The branches below that *do* change the scene (a window dragged by
    // a grab, a drag icon following the pointer) set `dirty` for themselves.
    state.cursor_dirty = true;
    let (x, y) = (state.cursor_x, state.cursor_y);

    // A grab owns the pointer: motion drives the window, and the client hears
    // nothing until the button is up.
    if update_grab(state, x, y) {
        state.dirty = true;
        return;
    }

    // A drag owns it for the same reason, and delivers to the surface
    // underneath through its data device rather than its pointer. The drag icon
    // is a scene element that follows the pointer, so the scene changes.
    if update_drag(state, time_ms) {
        state.dirty = true;
        return;
    }

    // Auto-focus the top surface if nothing is focused yet
    if state.focused_surface.is_none()
        && let Some(top_key) = state.top_visible_toplevel()
        && state.surfaces.contains_key(&top_key)
    {
        switch_focus(state, top_key);
    }

    // Determine which specific surface the pointer is over
    let hit = hit_test(state, x, y);
    let new_pointer_surface = hit.as_ref().map(|h| h.surface);
    let old_pointer_surface = state.pointer_surface;

    // Send pointer enter/leave when the surface under the cursor changes
    if new_pointer_surface != old_pointer_surface {
        if let Some(old_ps) = old_pointer_surface {
            for ptr in state.pointers.clone() {
                if ptr.client_id == old_ps.0 {
                    wl_pointer::send_leave(state, ptr.client_id, ptr.object_id, old_ps.1);
                    wl_pointer::send_frame(state, ptr.client_id, ptr.object_id);
                }
            }
        }
        state.pointer_surface = new_pointer_surface;
        if let Some(ref h) = hit {
            let local_x = x - f64::from(h.surface_x);
            let local_y = y - f64::from(h.surface_y);
            for ptr in state.pointers.clone() {
                if ptr.client_id == h.surface.0 {
                    wl_pointer::send_enter(
                        state,
                        ptr.client_id,
                        ptr.object_id,
                        h.surface.1,
                        local_x,
                        local_y,
                    );
                    wl_pointer::send_frame(state, ptr.client_id, ptr.object_id);
                }
            }
        }
    }

    // Send motion to the current pointer surface
    if let Some(ref h) = hit {
        let local_x = x - f64::from(h.surface_x);
        let local_y = y - f64::from(h.surface_y);
        for ptr in state.pointers.clone() {
            if ptr.client_id == h.surface.0 {
                wl_pointer::send_motion(
                    state,
                    ptr.client_id,
                    ptr.object_id,
                    time_ms,
                    local_x,
                    local_y,
                );
                wl_pointer::send_frame(state, ptr.client_id, ptr.object_id);
            }
        }
    }
}

/// Raise the window under a click, bringing any dialogs of its own with it.
///
/// Goes through the toplevel rather than straight to the workspace stack so
/// that `set_parent` is honoured — a dialog must not be buried by a click on
/// the window it belongs to.
pub fn accepts_click_focus(state: &CompositorState, surface: ClientObjectId) -> bool {
    let Some(layer_key) = state.layer_surface_of(surface) else {
        return true;
    };
    state.layer_surfaces.get(&layer_key).is_some_and(|layer| {
        layer.current.keyboard_interactivity != state::KeyboardInteractivity::None
    })
}

/// A finger has landed. Find what it landed on and tell that client.
///
/// The surface is settled here and remembered for the life of the point, so
/// every later motion and the eventual lift go to the same client however far
/// the finger travels.
pub fn touch_down(state: &mut CompositorState, time_ms: u32, id: i32, x: f64, y: f64) {
    state.dirty = true;
    let Some(hit) = hit_test(state, x, y) else {
        // Nothing there. The point is not recorded, so its motion and lift are
        // dropped too rather than being delivered to whatever is touched next.
        return;
    };

    // Touching a window raises and focuses it — or does whatever else the
    // shell decides a click on it should, since the two are the same gesture.
    state.with_shell(|shell, state| shell.toplevel_clicked(state, &hit));

    state.touch_points.insert(id, hit.surface);
    let (local_x, local_y) = (x - f64::from(hit.surface_x), y - f64::from(hit.surface_y));
    for touch_id in wl_touch::touches_of(state, hit.surface.0) {
        wl_touch::send_down(
            state,
            hit.surface.0,
            touch_id,
            time_ms,
            hit.surface.1,
            id,
            local_x,
            local_y,
        );
        wl_touch::send_frame(state, hit.surface.0, touch_id);
    }
}

/// A finger already down has moved, in the coordinates of the surface it
/// started on — which may be nowhere near where it is now.
pub fn touch_motion(state: &mut CompositorState, time_ms: u32, id: i32, x: f64, y: f64) {
    state.dirty = true;
    let Some(&surface) = state.touch_points.get(&id) else {
        return;
    };
    let origin = surface_global_position(state, surface.0, surface.1);
    let (local_x, local_y) = (x - f64::from(origin.0), y - f64::from(origin.1));
    for touch_id in wl_touch::touches_of(state, surface.0) {
        wl_touch::send_motion(state, surface.0, touch_id, time_ms, id, local_x, local_y);
        wl_touch::send_frame(state, surface.0, touch_id);
    }
}

/// A finger has been lifted.
pub fn touch_up(state: &mut CompositorState, time_ms: u32, id: i32) {
    state.dirty = true;
    let Some(surface) = state.touch_points.remove(&id) else {
        return;
    };
    for touch_id in wl_touch::touches_of(state, surface.0) {
        wl_touch::send_up(state, surface.0, touch_id, time_ms, id);
        wl_touch::send_frame(state, surface.0, touch_id);
    }
}

/// The touch sequence has been taken over, so every point in it is void.
///
/// Every client holding a point is told, not just the one under the last
/// finger: a two-finger gesture spanning two windows leaves both of them
/// waiting to be told how it ended.
pub fn touch_cancel(state: &mut CompositorState) {
    state.dirty = true;
    let mut clients: Vec<u32> = state.touch_points.values().map(|s| s.0).collect();
    clients.sort_unstable();
    clients.dedup();
    state.touch_points.clear();
    for client_id in clients {
        for touch_id in wl_touch::touches_of(state, client_id) {
            wl_touch::send_cancel(state, client_id, touch_id);
        }
    }
}

/// Deliver a scroll to whichever client has the pointer.
///
/// The order within the frame is the protocol's and is not arbitrary: the
/// source first, so a client knows what kind of scroll it is about to be told
/// about; then the detent count for an axis, before the distance it explains;
/// then the distance; then the frame that says the picture is complete. A
/// client acting on the distance before it knows the source cannot decide
/// whether the scroll may have momentum.
pub fn deliver_scroll(
    state: &mut CompositorState,
    time_ms: u32,
    dx: f64,
    dy: f64,
    source: ScrollSource,
    v120_x: i32,
    v120_y: i32,
) {
    let Some((pointer_client, _)) = state.pointer_surface else {
        return;
    };

    // Natural scrolling and the scroll factor only mean something for a
    // touchpad: a wheel has no "direction the content moves" convention of
    // its own to flip, and no distance of its own to scale — only detents.
    let touchpad = source == ScrollSource::Finger;
    let flip = touchpad && !state.settings.natural_scrolling;
    let scale = if touchpad {
        state.settings.scroll_factor
    } else {
        1.0
    };
    let (dx, dy) = if flip { (-dx, -dy) } else { (dx, dy) };
    let (dx, dy) = (dx * scale, dy * scale);
    let (v120_x, v120_y) = if flip {
        (-v120_x, -v120_y)
    } else {
        (v120_x, v120_y)
    };

    // A touchpad scroll ends, and the axes it was moving are the ones that have
    // to be told so. A wheel never stops in this sense — it is between detents,
    // not finished.
    if source == ScrollSource::Finger {
        state.scrolling_vertical |= dy != 0.0;
        state.scrolling_horizontal |= dx != 0.0;
    }

    for ptr in state.pointers.clone() {
        if ptr.client_id != pointer_client {
            continue;
        }
        wl_pointer::send_axis_source(state, ptr.client_id, ptr.object_id, source);
        if dy != 0.0 {
            wl_pointer::send_axis_steps(
                state,
                ptr.client_id,
                ptr.object_id,
                wl_pointer::AXIS_VERTICAL,
                v120_y,
            );
            wl_pointer::send_axis(
                state,
                ptr.client_id,
                ptr.object_id,
                time_ms,
                wl_pointer::AXIS_VERTICAL,
                dy * SCROLL_STEP,
            );
        }
        if dx != 0.0 {
            wl_pointer::send_axis_steps(
                state,
                ptr.client_id,
                ptr.object_id,
                wl_pointer::AXIS_HORIZONTAL,
                v120_x,
            );
            wl_pointer::send_axis(
                state,
                ptr.client_id,
                ptr.object_id,
                time_ms,
                wl_pointer::AXIS_HORIZONTAL,
                dx * SCROLL_STEP,
            );
        }
        wl_pointer::send_frame(state, ptr.client_id, ptr.object_id);
    }
}

/// Tell the client a touchpad scroll has finished.
///
/// Only the axes that were actually moving are stopped: an `axis_stop` for an
/// axis that never scrolled is a statement about something that never happened.
pub fn deliver_scroll_end(state: &mut CompositorState, time_ms: u32) {
    let (vertical, horizontal) = (state.scrolling_vertical, state.scrolling_horizontal);
    state.scrolling_vertical = false;
    state.scrolling_horizontal = false;
    if !vertical && !horizontal {
        return;
    }
    let Some((pointer_client, _)) = state.pointer_surface else {
        return;
    };

    for ptr in state.pointers.clone() {
        if ptr.client_id != pointer_client {
            continue;
        }
        wl_pointer::send_axis_source(state, ptr.client_id, ptr.object_id, ScrollSource::Finger);
        if vertical {
            wl_pointer::send_axis_stop(
                state,
                ptr.client_id,
                ptr.object_id,
                time_ms,
                wl_pointer::AXIS_VERTICAL,
            );
        }
        if horizontal {
            wl_pointer::send_axis_stop(
                state,
                ptr.client_id,
                ptr.object_id,
                time_ms,
                wl_pointer::AXIS_HORIZONTAL,
            );
        }
        wl_pointer::send_frame(state, ptr.client_id, ptr.object_id);
    }
}

/// Tell clients which outputs each of their surfaces is displayed on.
///
/// A client cannot choose a buffer scale without this: `wl_output.scale` is per
/// output, so the surface has to know which outputs it overlaps. The full set is
/// recomputed and diffed against what each surface has already been told, so
/// this is idempotent and safe to run every frame.
pub fn update_surface_outputs(state: &mut CompositorState) {
    let mut enters: Vec<(u32, u32, OutputId)> = Vec::new();
    let mut leaves: Vec<(u32, u32, OutputId)> = Vec::new();
    // What is actually on screen, which is not what the client has been told:
    // an output it has not bound is one it cannot be told about. Collected
    // here and stored below, since the loop only has the surfaces borrowed.
    let mut visible: Vec<((u32, u32), HashSet<OutputId>)> = Vec::new();

    for (&(client_id, surface_id), surface) in &state.surfaces {
        // An unmapped surface is on no output, and neither is one belonging to
        // a window on a workspace that is not showing.
        if surface.buffer_id.is_none() || !is_visible(state, (client_id, surface_id)) {
            leaves.extend(
                surface
                    .entered_outputs
                    .iter()
                    .map(|&output_id| (client_id, surface_id, output_id)),
            );
            visible.push(((client_id, surface_id), HashSet::new()));
            continue;
        }

        let (x, y) = surface_global_position(state, client_id, surface_id);
        let (w, h) = state.surface_size((client_id, surface_id));
        // A zero size here is not geometry, it is the destroyed-buffer gap: a
        // client mid-resize tears its old buffer down before committing the
        // replacement, and for that instant the size cannot be computed.
        // Acting on it would bounce the surface off its outputs and back —
        // leave/enter storms and preferred-scale flapping, dozens of times
        // per resize — so the last real answer stands until there is a new
        // one.
        if w <= 0 || h <= 0 {
            continue;
        }

        let mut on = HashSet::new();
        for output in &state.outputs {
            let geometry = &output.geometry;
            // Logical, like the surface position and size it is compared with.
            let (output_width, output_height) = output.logical_size();
            let overlaps = x < geometry.x + output_width
                && x.saturating_add(w) > geometry.x
                && y < geometry.y + output_height
                && y.saturating_add(h) > geometry.y;
            if overlaps {
                on.insert(output.id);
            }
            match (overlaps, surface.entered_outputs.contains(&output.id)) {
                (true, false) => enters.push((client_id, surface_id, output.id)),
                (false, true) => leaves.push((client_id, surface_id, output.id)),
                _ => {}
            }
        }
        visible.push(((client_id, surface_id), on));
    }

    for (key, on) in visible {
        if let Some(surface) = state.surfaces.get_mut(&key) {
            surface.visible_on = on;
        }
    }

    for (client_id, surface_id, output_id) in enters {
        let objects = bound_output_objects(state, client_id, output_id);
        // A client that has not bound this output has no object we could name,
        // so leave the surface unmarked and try again once it binds.
        if objects.is_empty() {
            continue;
        }
        for object_id in objects {
            wl_surface::send_enter(state, client_id, surface_id, object_id);
        }
        if let Some(surface) = state.surfaces.get_mut(&(client_id, surface_id)) {
            surface.entered_outputs.insert(output_id);
        }
    }

    for (client_id, surface_id, output_id) in leaves {
        for object_id in bound_output_objects(state, client_id, output_id) {
            wl_surface::send_leave(state, client_id, surface_id, object_id);
        }
        if let Some(surface) = state.surfaces.get_mut(&(client_id, surface_id)) {
            surface.entered_outputs.remove(&output_id);
        }
    }

    update_fractional_scales(state);
}

/// Tell each surface that asked what scale it should draw itself at.
///
/// Here rather than anywhere else because it answers the same question
/// [`update_surface_outputs`] has just worked out — which outputs a surface is
/// on — so the two cannot disagree about it. Run every frame and diffed against
/// the last answer, which covers every way the answer can change: a surface
/// moving between outputs, an output reconfigured to a different scale, a window
/// mapping for the first time.
fn update_fractional_scales(state: &mut CompositorState) {
    let changed: Vec<(ClientObjectId, u32)> = state
        .fractional_scales
        .iter()
        .filter_map(|(&object, binding)| {
            let scale = state
                .preferred_scale((object.0, binding.surface_id))
                .as_120ths();
            (binding.sent != Some(scale)).then_some((object, scale))
        })
        .collect();

    for (object, scale) in changed {
        wp_fractional_scale::send_preferred_scale(state, object, scale);
        if let Some(binding) = state.fractional_scales.get_mut(&object) {
            binding.sent = Some(scale);
        }
    }
}

/// Format names and modifier counts, for a log line.
/// Whether a surface is part of a window the user can currently see.
///
/// Popups and subsurfaces have no standing of their own, so the question is
/// really about the toplevel they hang off: walking up to the root surface is
/// what turns one into the other, and the shell answers for the root.
pub fn is_visible(state: &CompositorState, key: ClientObjectId) -> bool {
    state.shell.is_visible(root_surface(state, key))
}

/// Walk up the parent chain to the surface at the root of the tree.
fn root_surface(state: &CompositorState, key: ClientObjectId) -> ClientObjectId {
    let (client_id, mut current) = key;
    // Bounded by the number of surfaces: the parent links form a tree, and
    // `wl_subcompositor` rejects a cycle when the link is made.
    while let Some(parent) = state
        .surfaces
        .get(&(client_id, current))
        .and_then(|s| s.parent)
    {
        current = parent;
    }
    (client_id, current)
}

/// The client's `wl_output` object ids that refer to a given output. A client
/// may bind the same output more than once, and each binding is a distinct
/// object the events have to be sent for.
fn bound_output_objects(state: &CompositorState, client_id: u32, output_id: OutputId) -> Vec<u32> {
    state
        .output_bindings
        .iter()
        .filter(|&(&(cid, _), &oid)| cid == client_id && oid == output_id)
        .map(|(&(_, object_id), _)| object_id)
        .collect()
}

/// Ask the focused toplevel to close.
///
/// Only a request — the client decides whether to honour it, so the window is
/// torn down later through the normal `xdg_toplevel.destroy` path, not here.
pub fn clear_focus(state: &mut CompositorState) {
    let Some((client_id, surface_id)) = state.focused_surface.take() else {
        return;
    };
    for kb in state.keyboards.clone() {
        if kb.client_id == client_id {
            wl_keyboard::send_leave(state, client_id, kb.object_id, surface_id);
        }
    }
    xdg_toplevel::send_activated(state, client_id, surface_id, false);
}

/// Run a program named by a `spawn` binding's `program` field.
///
/// Split on whitespace only — there is no shell in between, so quoting is not
/// respected, which is enough for a command plus a few flags but not for an
/// argument containing a space. Fire-and-forget: nothing here waits on the
/// child, since there is no client to report success or failure to, and
/// Tokio reaps it in the background once it exits.
///
/// It inherits this process's environment, which is what gets it
/// `WAYLAND_DISPLAY` — see `main`, which sets that before any subsystem
/// (including this one) starts.
pub(crate) struct Presentation {
    /// When the frame reached the screen.
    pub(crate) at: MonotonicTimeStamp,
    /// Nominal refresh interval in nanoseconds, 0 if unknown.
    pub(crate) refresh_ns: u32,
    /// The output's refresh counter, 0 if the backend cannot supply one.
    pub(crate) sequence: u64,
    /// What the backend can vouch for about how it presented.
    pub(crate) flags: tokio_way_backends::messages::PresentationFlags,
}

/// Fire the pending frame callbacks and presentation feedbacks of the surfaces
/// `target` covers.
///
/// `presented` describes the frame reaching the screen, and `None` says nothing
/// was put anywhere — which is the honest answer for a surface no output is
/// showing, and makes its presentation feedback `discarded` rather than a
/// `presented` naming a time nothing happened.
pub(crate) fn fire_frame_callbacks(
    state: &mut CompositorState,
    timestamp_ms: u32,
    target: FrameTarget,
    presented: Option<Presentation>,
) {
    let mut callbacks: Vec<(u32, u32)> = Vec::new(); // (client_id, callback_id)
    let mut presentation: Vec<(u32, u32)> = Vec::new(); // (client_id, feedback_id)

    for surface in state.surfaces.values_mut() {
        // `visible_on` rather than `entered_outputs`: the latter is what the
        // client has been told, and a client that never bound `wl_output` has
        // been told nothing. Pacing it on that would leave it waiting on a
        // callback that could not arrive.
        let covered = match target {
            FrameTarget::Output(output_id) => surface.visible_on.contains(&output_id),
            FrameTarget::Offscreen => surface.visible_on.is_empty(),
        };
        if !covered {
            continue;
        }
        for callback_id in surface.frame_callbacks.drain(..) {
            callbacks.push((surface.client_id, callback_id));
        }
        for feedback_id in surface.presentation_feedbacks.drain(..) {
            presentation.push((surface.client_id, feedback_id));
        }
    }

    // Fire wl_callback.done events with timestamp
    for (client_id, callback_id) in callbacks {
        if let Some(client) = state.clients.get(client_id) {
            let args = ArgWriter::new().u32(timestamp_ms).build();
            let _ = client.send(build_message(callback_id, 0, args));
            client.unregister(callback_id);
        } else {
            debug!(
                "Client {} disappeared before frame callback could be fired",
                client_id
            );
        }
    }

    // Nothing reached a screen, so every feedback is discarded rather than
    // presented. A client hearing `presented` with a timestamp for a frame
    // that was never shown would be told a straightforward untruth, and the
    // protocol exists precisely to be accurate about this.
    let Some(presented) = presented else {
        for (client_id, feedback_id) in presentation {
            if let Some(client) = state.clients.get(client_id) {
                let _ = client.send(build_message(
                    feedback_id,
                    wp_presentation_feedback::DISCARDED,
                    Vec::new(),
                ));
                client.unregister(feedback_id);
            }
        }
        return;
    };

    // Fire wp_presentation_feedback.presented events
    if !presentation.is_empty() {
        // The backend's own clock reading, taken when it presented, rather than
        // one measured here a channel hop later. Accurate timing is the whole
        // point of this protocol.
        // A 64-bit second count split across two 32-bit arguments, so the low
        // half is the low half — truncated, not range-checked. `try_from` here
        // answered zero for anything past `u32::MAX` while the high half went
        // out correctly, which is a timestamp off by however many seconds the
        // clock had run rather than one the client can tell is wrong.
        let seconds = presented.at.tv_sec.cast_unsigned();
        let tv_sec_hi = u32::try_from(seconds >> 32).unwrap_or(u32::MAX);
        // Truncation is the operation, not a risk of it: this argument *is*
        // the low 32 bits, and the other half carries the rest.
        #[allow(clippy::cast_possible_truncation)]
        let tv_sec_lo = seconds as u32;
        let tv_nsec = u32::try_from(presented.at.tv_nsec).unwrap_or(0);

        // The output's refresh counter, split the same way, low half truncated.
        #[allow(clippy::cast_possible_truncation)]
        let seq_lo = presented.sequence as u32;
        let seq_hi = u32::try_from(presented.sequence >> 32).unwrap_or(u32::MAX);

        // What the frame was presented with, as told by the backend rather than
        // assumed here. A hosted backend can vouch for none of these — its
        // frame passes through a host compositor it does not drive — so it
        // reports the default and the mask stays empty, which is the honest
        // answer. A DRM backend scanning out a page flip sets them, and the
        // client learns the timing was real. Deciding this is the backend's
        // job, not the compositor's: only it knows how the pixels got out.
        let flags = wp_presentation_feedback::kind_mask(presented.flags);
        for (client_id, feedback_id) in presentation {
            let args = ArgWriter::new()
                .u32(tv_sec_hi)
                .u32(tv_sec_lo)
                .u32(tv_nsec)
                .u32(presented.refresh_ns)
                .u32(seq_hi)
                .u32(seq_lo)
                .u32(flags)
                .build();
            if let Some(client) = state.clients.get(client_id) {
                // Opcode 1. Sending this payload as opcode 0 makes the
                // client decode it as `sync_output`, whose single argument is
                // a non-nullable object — a zero there is a fatal decode error
                // that takes the connection down.
                let _ = client.send(build_message(
                    feedback_id,
                    wp_presentation_feedback::PRESENTED,
                    args,
                ));
                client.unregister(feedback_id);
            } else {
                debug!(
                    "Client {} disappeared before presentation feedback could be fired",
                    client_id
                );
            }
        }
    }
}

/// Note buffers a commit replaced as no longer wanted.
///
/// This is not the release itself: a frame still in flight may be reading the
/// buffer, and telling the client it may draw would corrupt what is on screen.
/// Buffers still attached to a surface are skipped, which happens when several
/// commits are batched and a buffer is re-attached after being replaced.
pub fn start_buffer_releases(state: &mut CompositorState) {
    for (client_id, buffer_id) in std::mem::take(&mut state.buffers_pending_release) {
        let still_attached = state
            .surfaces
            .values()
            .any(|s| s.client_id == client_id && s.buffer_id == Some(buffer_id));
        if still_attached {
            continue;
        }
        state.releasing_buffers.insert((client_id, buffer_id));
    }
}

/// Tell clients about buffers nothing is reading any more.
///
/// Runs every tick, not only the ones that draw: the last reader is usually the
/// previous frame, which is dropped when a later frame replaces it or the
/// backend finishes with it, and neither is tied to this compositor's idea of
/// whether anything changed.
pub fn finish_buffer_releases(state: &mut CompositorState) {
    if state.releasing_buffers.is_empty() {
        return;
    }
    let buffers_to_release: Vec<ClientObjectId> = state
        .releasing_buffers
        .iter()
        .filter(|&&key| !state.buffer_is_being_read(key))
        // A client can re-attach a buffer before hearing it was released.
        // Telling it the buffer is free while it is on screen would invite it
        // to draw over what is being displayed.
        .filter(|&&(client_id, buffer_id)| {
            !state
                .surfaces
                .values()
                .any(|s| s.client_id == client_id && s.buffer_id == Some(buffer_id))
        })
        .copied()
        .collect();
    for key in &buffers_to_release {
        state.releasing_buffers.remove(key);
    }
    // The last gate before the wire: a buffer that no longer exists is never
    // released, whichever queue it slipped through. The id may already name a
    // different object on the client's side, and an event to an object of the
    // wrong interface kills the whole connection.
    let buffers_to_release: Vec<ClientObjectId> = buffers_to_release
        .into_iter()
        .filter(|key| state.buffers.contains_key(key))
        .collect();

    // Send wl_buffer.release (opcode 0, no args)
    for (client_id, buffer_id) in buffers_to_release {
        if let Some(client) = state.clients.get(client_id) {
            debug!("wl_buffer.release -> {buffer_id}");
            let _ = client.send(build_message(buffer_id, 0, Vec::new()));
        } else {
            debug!(
                "Client {} disappeared before buffer release could be sent",
                client_id
            );
        }
    }
}

/// The size range an interactive resize of a window may produce.
///
/// Two sources, and the narrowest wins where they overlap:
///
/// - the compositor's own floor, so the window stays grabbable;
/// - the client's `set_min_size` and `set_max_size`, which say what it can
///   actually render at.
///
/// Nothing here bounds a resize to the output's size. A window may grow past
/// its display's edge same as it may be dragged past it; what falls outside
/// is simply not drawn, the way any element positioned off its output isn't.
///
/// Where the client's own limits contradict each other the range is widened
/// rather than inverted, because a clamp needs a floor no higher than its
/// ceiling. A client that insists on a minimum larger than its stated maximum
/// gets the minimum: the compositor cannot make it render smaller, and
/// configuring a size it will refuse achieves nothing.
pub fn resize_limits(state: &CompositorState, surface: ClientObjectId) -> ((i32, i32), (i32, i32)) {
    let ((client_min_w, client_min_h), (client_max_w, client_max_h)) =
        state.client_size_limits(surface);

    let min_width = MIN_WINDOW_WIDTH.max(client_min_w);
    let min_height = MIN_WINDOW_HEIGHT.max(client_min_h);
    // Zero means the client named no maximum, so there is none.
    let max_width = (if client_max_w > 0 {
        client_max_w
    } else {
        i32::MAX
    })
    .max(min_width);
    let max_height = (if client_max_h > 0 {
        client_max_h
    } else {
        i32::MAX
    })
    .max(min_height);

    ((min_width, min_height), (max_width, max_height))
}

/// Drive the active grab from a pointer position. Returns false if there is none.
pub fn update_grab(state: &mut CompositorState, x: f64, y: f64) -> bool {
    let Some(grab) = state.pointer_grab else {
        return false;
    };
    match grab.kind {
        GrabKind::Move { offset_x, offset_y } => {
            let position = (f64_to_i32(x) + offset_x, f64_to_i32(y) + offset_y);
            if let Some(surface) = state.surfaces.get_mut(&grab.surface) {
                surface.position = position;
            }
            // Dragging toward another output hands the window's workspace
            // membership over rather than leaving it on the one it started
            // on: a window belongs to whichever output the pointer is over,
            // however far its own bounds still reach onto the one it left.
            let pointer_output = state
                .outputs
                .iter()
                .find(|o| output_contains(o, f64_to_i32(x), f64_to_i32(y)))
                .map(|o| o.id);
            if let Some(output_id) = pointer_output {
                let surface = grab.surface;
                state.with_shell(|shell, state| {
                    shell.toplevel_dragged_to_output(state, surface, output_id);
                });
            }
        }
        GrabKind::Resize {
            edges,
            start_pointer,
            start_size,
            last_sent,
        } => {
            let dx = f64_to_i32(x - start_pointer.0);
            let dy = f64_to_i32(y - start_pointer.1);

            let ((min_width, min_height), (max_width, max_height)) =
                resize_limits(state, grab.surface);

            // Each edge moves independently, and the opposite edge stays
            // put. Only the *size* is computed here: the position of a left-
            // or top-edge resize follows the sizes the client actually
            // commits, via the resize anchor — moving it now, against a
            // buffer still the old size, would wobble the anchored edge by
            // the client's lag on every frame.
            let mut width = start_size.0;
            let mut height = start_size.1;
            if edges.right() {
                width = (start_size.0 + dx).clamp(min_width, max_width);
            } else if edges.left() {
                width = (start_size.0 - dx).clamp(min_width, max_width);
            }
            if edges.bottom() {
                height = (start_size.1 + dy).clamp(min_height, max_height);
            } else if edges.top() {
                height = (start_size.1 - dy).clamp(min_height, max_height);
            }

            // The client decides its own size; all we can do is ask. Asking on
            // every motion event would flood it, so an unchanged size is
            // silence.
            if (width, height) != last_sent {
                if let Some(g) = state.pointer_grab.as_mut()
                    && let GrabKind::Resize { last_sent, .. } = &mut g.kind
                {
                    *last_sent = (width, height);
                }
                configure_resizing(state, grab.toplevel, width, height, true);
            }
        }
    }
    state.dirty = true;
    true
}

/// End the active grab, telling a resized client it has stopped resizing.
pub fn end_grab(state: &mut CompositorState) {
    let Some(grab) = state.pointer_grab.take() else {
        return;
    };
    if let GrabKind::Resize { last_sent, .. } = grab.kind {
        configure_resizing(state, grab.toplevel, last_sent.0, last_sent.1, false);
    }
    state.dirty = true;
}

/// Deliver a drag to whatever is under the pointer. Returns false if there is
/// no drag, in which case the pointer belongs to the ordinary path.
///
/// The surface under the pointer hears about this through its *data device*,
/// not its pointer: no `wl_pointer` event reaches anyone for the duration of a
/// drag, and the target learns where the pointer is from
/// `wl_data_device.motion`.
pub fn update_drag(state: &mut CompositorState, time_ms: u32) -> bool {
    let Some(drag) = state.drag.clone() else {
        return false;
    };
    let (x, y) = (state.cursor_x, state.cursor_y);
    state.dirty = true;

    // A drag with no source is one client dragging within itself. It has
    // nothing to hand anyone else, so nobody else is a target — filtering here
    // rather than at delivery keeps the enter and leave bookkeeping honest,
    // since the drag never considers itself to have entered a surface it may
    // not talk to.
    let hit = hit_test(state, x, y)
        .filter(|h| drag.source.is_some() || h.surface.0 == drag.origin_client);
    let new_focus = hit.as_ref().map(|h| h.surface);

    if new_focus == drag.focus {
        if let Some(h) = hit {
            let (local_x, local_y) = (x - f64::from(h.surface_x), y - f64::from(h.surface_y));
            for device_id in wl_data_device::devices_of(state, h.surface.0) {
                wl_data_device::send_motion(
                    state,
                    h.surface.0,
                    device_id,
                    time_ms,
                    local_x,
                    local_y,
                );
            }
        }
        return true;
    }

    state.end_drag_focus();
    if let Some(h) = hit {
        let (local_x, local_y) = (x - f64::from(h.surface_x), y - f64::from(h.surface_y));
        enter_drag_surface(state, h.surface, local_x, local_y);
    }
    true
}

/// Introduce a drag to the client whose surface it has just moved over.
///
/// One offer per data device, because `enter` is a per-device event and an
/// offer belongs to the device it arrived on. The order within a device is
/// forced: the client has to know the object exists and what mime types are
/// behind it before it is told a drag is over it and asked to decide.
pub fn enter_drag_surface(state: &mut CompositorState, surface: ClientObjectId, x: f64, y: f64) {
    let Some(source) = state.drag.as_ref().map(|drag| drag.source) else {
        return;
    };
    let client_id = surface.0;
    let source_actions = source
        .and_then(|key| state.data_sources.get(&key))
        .map_or(0, |s| s.actions);

    let mut offers = Vec::new();
    for device_id in wl_data_device::devices_of(state, client_id) {
        let offer = source.and_then(|source| {
            wl_data_device::create_offer(state, client_id, device_id, source, OfferKind::Drag)
        });
        if let Some(offer) = offer {
            // Before the enter, so the client has the whole picture at the
            // moment it decides what it will accept.
            wl_data_offer::send_source_actions(state, offer, source_actions);
            offers.push(offer);
        }
        wl_data_device::send_enter(
            state,
            client_id,
            device_id,
            surface.1,
            x,
            y,
            offer.map(|(_, offer_id)| offer_id),
        );
    }

    if let Some(drag) = state.drag.as_mut() {
        drag.focus = Some(surface);
        drag.focus_offers.clone_from(&offers);
    }
    // After the enter, so the client has already been told the offer exists
    // before it hears what action it would settle on.
    for offer in offers {
        state.resolve_offer_action(offer);
    }
}

/// Resolve a drag when the button comes up.
///
/// A drop frees the pointer immediately and does *not* end the offer. The
/// target has still to read the data and say when it is done, and everything
/// that needs is reachable from the offer rather than from the drag — so the
/// drag is what ends here and the offer is what survives. Keeping the drag
/// alive to mean "dropped but not finished" would put a second condition on
/// every check of whether the pointer is spoken for, and the one place that
/// forgot it would swallow the pointer for good.
pub fn finish_drag(state: &mut CompositorState) {
    // Read before the drag is taken: this asks what the target accepted
    // through it.
    let accepted = state.drag_target_accepted();
    let Some(drag) = state.drag.take() else {
        return;
    };
    state.dirty = true;

    let Some(focus) = drag.focus else {
        // Let go over nothing. There is nobody to drop on, so the source is
        // told the drag came to nothing.
        if let Some(source) = drag.source {
            cancel_source(state, source);
        }
        return;
    };

    // A drag with no source went only to the client that started it, which
    // handles the content itself: there is no source to tell and no offer to
    // keep alive.
    if drag.source.is_none() {
        for device_id in wl_data_device::devices_of(state, focus.0) {
            wl_data_device::send_drop(state, focus.0, device_id);
        }
        return;
    }

    if !accepted {
        for device_id in wl_data_device::devices_of(state, focus.0) {
            wl_data_device::send_leave(state, focus.0, device_id);
        }
        for &offer in &drag.focus_offers {
            state.invalidate_offer(offer);
        }
        if let Some(source) = drag.source {
            cancel_source(state, source);
        }
        return;
    }

    for device_id in wl_data_device::devices_of(state, focus.0) {
        wl_data_device::send_drop(state, focus.0, device_id);
    }
    for &offer in &drag.focus_offers {
        state.mark_offer_dropped(offer);
    }
    if let Some(source) = drag.source {
        wl_data_source::send_dnd_drop_performed(state, source);
    }
}

/// Ask a client for a size, marking whether the resize is still in progress.
fn configure_resizing(
    state: &mut CompositorState,
    toplevel: ClientObjectId,
    width: i32,
    height: i32,
    resizing: bool,
) {
    xdg_toplevel::send_resize_configure(state, toplevel.0, toplevel.1, width, height, resizing);
    // The size only takes effect once the client acknowledges the matching
    // xdg_surface.configure, so the two always travel together.
    if let Some(xdg_surface_id) = state.xdg_toplevels.get(&toplevel).map(|t| t.xdg_surface_id) {
        xdg_surface::send_configure(state, toplevel.0, xdg_surface_id);
    }
}

/// Which edges an Alt+drag resize should pull, from where in the window the
/// pointer sits: the nearest corner, so any part of the window is usable.
pub fn edges_for_point(
    state: &CompositorState,
    surface: ClientObjectId,
    x: f64,
    y: f64,
) -> ResizeEdges {
    let Some(position) = state.surfaces.get(&surface).map(|s| s.position) else {
        return ResizeEdges(ResizeEdges::BOTTOM | ResizeEdges::RIGHT);
    };
    let (width, height) = state.surface_size(surface);
    let horizontal = if f64_to_i32(x) < position.0 + width / 2 {
        ResizeEdges::LEFT
    } else {
        ResizeEdges::RIGHT
    };
    let vertical = if f64_to_i32(y) < position.1 + height / 2 {
        ResizeEdges::TOP
    } else {
        ResizeEdges::BOTTOM
    };
    ResizeEdges(horizontal | vertical)
}

/// Switch keyboard focus to a new toplevel surface. Sends keyboard enter/leave
/// and `xdg_toplevel` activated/deactivated configure events. Pointer focus is
/// tracked separately via `state.pointer_surface`.
pub fn switch_focus(state: &mut CompositorState, new_key: ClientObjectId) {
    if state.focused_surface == Some(new_key) {
        return;
    }

    clear_focus(state);

    // Send keyboard enter and activate the new focused surface
    state.focused_surface = Some(new_key);
    let new_client = new_key.0;
    let new_surface = new_key.1;
    for kb in state.keyboards.clone() {
        if kb.client_id == new_client {
            wl_keyboard::send_enter(state, new_client, kb.object_id, new_surface);
            // Immediately after the enter, which is the point the protocol
            // names: the client has just been handed the keyboard and has no
            // other way to learn that Alt is already down.
            wl_keyboard::send_current_modifiers(state, new_client, kb.object_id);
        }
    }
    xdg_toplevel::send_activated(state, new_client, new_surface, true);

    // The clipboard follows keyboard focus, so this is where a client is told
    // what is on it. After the keyboard enter, because having focus is what
    // makes the selection this client's to read.
    //
    // The client losing focus is told nothing: an offer it still holds stops
    // working the moment the selection is replaced, and it is not going to be
    // asked for a paste in the meantime.
    wl_data_device::send_selection_to_client(state, new_client);
    zwp_primary_selection_device::send_selection_to_client(state, new_client);
}

/// Every window on screen, topmost first.
///
/// Only the workspace showing on each output contributes: a window on a
/// workspace that is not displayed cannot be clicked. Windows are confined to
/// their own output and never straddle two, so the order between outputs
/// cannot matter — no two entries here overlap unless they share an output.
fn visible_toplevels_top_down(state: &CompositorState) -> Vec<ClientObjectId> {
    // The drawing order, reversed. A click lands on whatever is drawn last at
    // that point, so hit testing has to walk the same stack the other way — and
    // that stack now has three bands rather than one, with the windows in the
    // middle.
    let mut keys = Vec::new();
    for output in &state.outputs {
        // Over the windows, topmost first.
        let mut above = layer::keys_in_band(state, output.id, true);
        above.reverse();
        keys.extend(above);
    }
    let mut windows: Vec<ClientObjectId> = state
        .outputs
        .iter()
        .flat_map(|output| state.shell.visible_stack(output.id))
        .collect();
    windows.reverse();
    keys.extend(windows);
    for output in &state.outputs {
        let mut below = layer::keys_in_band(state, output.id, false);
        below.reverse();
        keys.extend(below);
    }
    keys
}

/// Where a surface the compositor places sits, in global logical coordinates.
///
/// A toplevel carries its own position; a layer surface's is computed from its
/// anchor and the room left over. Hit testing needs one answer for both.
fn placed_position(state: &CompositorState, key: ClientObjectId) -> (i32, i32) {
    if let Some(layer_key) = state.layer_surface_of(key)
        && let Some(geometry) = layer::geometry(state, layer_key)
    {
        return (geometry.x, geometry.y);
    }
    state.surfaces.get(&key).map_or((0, 0), |s| s.position)
}

/// Hit-test the visible windows from top to bottom. Returns the toplevel and
/// the specific surface (possibly a subsurface) under the pointer.
pub fn hit_test(state: &CompositorState, x: f64, y: f64) -> Option<HitResult> {
    let px = f64_to_i32(x);
    let py = f64_to_i32(y);

    for key in visible_toplevels_top_down(state) {
        if !state.surfaces.contains_key(&key) {
            continue;
        }
        // A layer surface that has not agreed a configure is not on screen, so
        // it must not be clickable either — the scene skips it for the same
        // reason.
        if let Some(layer_key) = state.layer_surface_of(key)
            && !state.layer_surfaces[&layer_key].configured
        {
            continue;
        }
        let (ox, oy) = placed_position(state, key);

        if let Some((surface_key, sx, sy)) = hit_test_surface_tree(state, key, ox, oy, px, py) {
            return Some(HitResult {
                toplevel: key,
                surface: surface_key,
                surface_x: sx,
                surface_y: sy,
            });
        }
    }
    None
}

/// Recursively hit-test a surface and its children at the given offset.
/// Returns the specific surface key and its global offset if hit.
fn hit_test_surface_tree(
    state: &CompositorState,
    surface_key: ClientObjectId,
    offset_x: i32,
    offset_y: i32,
    px: i32,
    py: i32,
) -> Option<(ClientObjectId, i32, i32)> {
    let surface = state.surfaces.get(&surface_key)?;
    let client_id = surface.client_id;
    let children = surface.children.clone();

    // Check children first (they render on top of the parent)
    for &child_id in children.iter().rev() {
        let child_key = (client_id, child_id);
        let Some(child) = state.surfaces.get(&child_key) else {
            continue;
        };
        let (cx, cy) = child.subsurface_position;
        if let Some(result) = hit_test_surface_tree(
            state,
            child_key,
            offset_x.saturating_add(cx),
            offset_y.saturating_add(cy),
            px,
            py,
        ) {
            return Some(result);
        }
    }

    // Check this surface's own bounds
    let (w, h) = state.surface_size(surface_key);
    if w == 0 || h == 0 {
        return None;
    }
    if px < offset_x
        || py < offset_y
        || px >= offset_x.saturating_add(w)
        || py >= offset_y.saturating_add(h)
    {
        return None;
    }

    // Within the surface's bounds, but the client may have narrowed which parts
    // of it accept pointer input. Clients drawing their own decorations use this
    // to let clicks fall through the drop shadow around the window.
    if let Some(input_region) = &surface.input_region
        && !region_contains(
            input_region,
            px.saturating_sub(offset_x),
            py.saturating_sub(offset_y),
        )
    {
        return None;
    }

    Some((surface_key, offset_x, offset_y))
}

/// Check if the pointer is outside the topmost grabbed popup. If so, dismiss
/// popups from the top of the grab stack until we reach one that contains the
/// pointer (or the stack is empty). Returns true if any popup was dismissed.
pub(crate) fn dismiss_popups_outside_click(state: &mut CompositorState) -> bool {
    let px = f64_to_i32(state.cursor_x);
    let py = f64_to_i32(state.cursor_y);
    let mut dismissed = false;

    while let Some(&(client_id, popup_id)) = state.grabbed_popups.last() {
        // Find the popup's wl_surface and compute its global position
        let popup_surface = state
            .xdg_popups
            .get(&(client_id, popup_id))
            .and_then(|p| state.xdg_surfaces.get(&(client_id, p.xdg_surface_id)))
            .map(|xs| xs.wl_surface_id);

        let Some(wl_surface_id) = popup_surface else {
            state.grabbed_popups.pop();
            continue;
        };

        // Walk up the parent chain to compute global position
        let global_pos = surface_global_position(state, client_id, wl_surface_id);
        let (w, h) = state.surface_size((client_id, wl_surface_id));

        if px >= global_pos.0
            && py >= global_pos.1
            && px < global_pos.0 + w
            && py < global_pos.1 + h
        {
            // Click is inside this popup — stop dismissing
            break;
        }

        // Click is outside — dismiss this popup
        state.grabbed_popups.pop();
        xdg_popup::send_popup_done(state, client_id, popup_id);
        dismissed = true;
    }

    dismissed
}

/// Compute the global position of a surface by walking up the parent chain.
fn surface_global_position(state: &CompositorState, client_id: u32, surface_id: u32) -> (i32, i32) {
    let mut x = 0i32;
    let mut y = 0i32;
    let mut current = surface_id;

    loop {
        let Some(surface) = state.surfaces.get(&(client_id, current)) else {
            break;
        };
        x = x.saturating_add(surface.subsurface_position.0);
        y = y.saturating_add(surface.subsurface_position.1);
        if let Some(parent_id) = surface.parent {
            current = parent_id;
        } else {
            // Root surface — add its global position
            x = x.saturating_add(surface.position.0);
            y = y.saturating_add(surface.position.1);
            break;
        }
    }

    (x, y)
}
