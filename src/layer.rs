//! Where a layer surface goes, and what room it leaves behind.
//!
//! A layer surface is placed by the compositor rather than by itself: it names
//! the edges it wants to stick to, a size, and some margins, and the
//! compositor works out the rectangle. That arithmetic is here rather than in
//! the protocol handler because two separate things need it — placing the
//! surface, and working out how much of the output is left for windows — and
//! they must agree, or a panel reserves space somewhere it is not.

use super::state::{
    Anchor, ClientObjectId, CompositorState, LayerSurfaceState, LayerSurfaceStatePending,
};
use tokio_way_backends::outputs::{Output, OutputId};

/// A rectangle in the output's logical coordinates, relative to its origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    /// The whole of an output, which is where placement starts from.
    fn of_output(output: &Output) -> Self {
        let (width, height) = output.logical_size();
        Self {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    /// Take `amount` off one edge, as an exclusive zone does.
    fn shrink(self, anchor: Anchor, amount: i32) -> Self {
        let amount = amount.max(0);
        // Which edge to take it from is decided by the single edge the surface
        // is anchored to. A surface spanning an axis reserves along the *other*
        // one — a full-width bar at the top takes height, not width.
        if anchor.spans_horizontally() && anchor.top() && !anchor.bottom() {
            return Self {
                y: self.y + amount,
                height: (self.height - amount).max(0),
                ..self
            };
        }
        if anchor.spans_horizontally() && anchor.bottom() && !anchor.top() {
            return Self {
                height: (self.height - amount).max(0),
                ..self
            };
        }
        if anchor.spans_vertically() && anchor.left() && !anchor.right() {
            return Self {
                x: self.x + amount,
                width: (self.width - amount).max(0),
                ..self
            };
        }
        if anchor.spans_vertically() && anchor.right() && !anchor.left() {
            return Self {
                width: (self.width - amount).max(0),
                ..self
            };
        }
        // Anchored to one edge without spanning, or to none at all: there is no
        // single edge to reserve from, so nothing is reserved. The protocol
        // leaves this to the compositor and taking a bite out of the middle of
        // the screen would serve nobody.
        self
    }
}

/// Every layer surface on an output placed, and what is left after them.
///
/// One pass, because placement and reservation are the same walk seen from two
/// ends and must not be computed apart. They were, once: `geometry` placed a
/// surface inside the area `usable_area` had already shrunk — including by
/// that surface's *own* exclusive zone — so a bar reserving 34 pixels at the
/// top was drawn 34 pixels down, holding a gap open for itself. A surface's
/// reservation is for everything placed after it and for the windows. Never
/// for itself.
///
/// Order is by layer and then by object id: stable across frames, so two bars
/// on the same edge stack outward and stay in the same order rather than
/// swapping every time the desktop is drawn.
pub struct Arrangement {
    placements: Vec<(ClientObjectId, Rect)>,
    /// What is left for ordinary windows once every reservation is taken.
    pub usable: Rect,
}

impl Arrangement {
    /// Where one layer surface was placed, in output-local coordinates.
    fn placement(&self, key: ClientObjectId) -> Option<Rect> {
        self.placements
            .iter()
            .find(|(placed, _)| *placed == key)
            .map(|(_, rect)| *rect)
    }
}

/// Place one layer surface inside the area available to it.
///
/// The rules are the protocol's. A surface anchored to both edges of an axis
/// stretches across it and may ask for size zero on that axis, meaning "as big
/// as the anchor makes me". Anchored to one edge, it sits against that edge at
/// the size it asked for. Anchored to neither, it is centred. Margins push it
/// away from the edges it is anchored to.
pub fn place(area: Rect, layer: &LayerSurfaceStatePending) -> Rect {
    let anchor = layer.anchor;
    let (margin_top, margin_right, margin_bottom, margin_left) = layer.margin;

    // The area the surface may occupy, once its margins are taken off the
    // edges it is anchored to. A margin on an edge it does not touch has
    // nothing to push against.
    let left = area.x + if anchor.left() { margin_left } else { 0 };
    let right = area.x + area.width - if anchor.right() { margin_right } else { 0 };
    let top = area.y + if anchor.top() { margin_top } else { 0 };
    let bottom = area.y + area.height - if anchor.bottom() { margin_bottom } else { 0 };
    let span_width = (right - left).max(0);
    let span_height = (bottom - top).max(0);

    // Size zero means "fill the anchored span". It is only meaningful when the
    // surface is anchored to both edges of that axis; otherwise there is no
    // span to fill and the surface has asked for nothing, so it gets nothing.
    let width = if layer.size.0 > 0 {
        layer.size.0
    } else if anchor.spans_horizontally() {
        span_width
    } else {
        0
    };
    let height = if layer.size.1 > 0 {
        layer.size.1
    } else if anchor.spans_vertically() {
        span_height
    } else {
        0
    };

    // And where it sits: against an edge it is anchored to, and centred on any
    // axis it is anchored to neither end of.
    let x = if anchor.left() && !anchor.right() {
        left
    } else if anchor.right() && !anchor.left() {
        right - width
    } else {
        left + (span_width - width) / 2
    };
    let y = if anchor.top() && !anchor.bottom() {
        top
    } else if anchor.bottom() && !anchor.top() {
        bottom - height
    } else {
        top + (span_height - height) / 2
    };

    Rect {
        x,
        y,
        width,
        height,
    }
}

pub fn arrange(state: &CompositorState, output_id: OutputId) -> Arrangement {
    let empty = Rect {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    };
    let Some(output) = state.outputs.iter().find(|o| o.id == output_id) else {
        return Arrangement {
            placements: Vec::new(),
            usable: empty,
        };
    };
    let full = Rect::of_output(output);
    let mut area = full;
    let mut placements = Vec::new();

    for (key, layer) in ordered_for_output(state, output_id) {
        // A negative exclusive zone means "I want no reservation and I ignore
        // everyone else's", which is how a wallpaper or a fullscreen overlay
        // asks for the whole output.
        let within = if layer.current.exclusive_zone < 0 {
            full
        } else {
            area
        };
        placements.push((key, place(within, &layer.current)));

        // And only now does its own reservation apply, to everything placed
        // after it.
        if layer.current.exclusive_zone > 0 {
            area = area.shrink(layer.current.anchor, layer.current.exclusive_zone);
        }
    }

    Arrangement {
        placements,
        usable: area,
    }
}

/// How much of an output is left for ordinary windows.
///
/// Every layer surface with a positive exclusive zone takes a strip off the
/// edge it is anchored to, and what is left is where a maximised window goes.
/// This is the whole reason a panel is not just a window drawn on top: without
/// it a maximised window sits under the bar and the user can reach neither.
pub fn usable_area(state: &CompositorState, output_id: OutputId) -> Rect {
    arrange(state, output_id).usable
}

/// Where a layer surface's `wl_surface` sits, in global logical coordinates.
pub fn geometry(state: &CompositorState, key: ClientObjectId) -> Option<Rect> {
    let output_id = state.layer_surfaces.get(&key)?.output?;
    let output = state.outputs.iter().find(|o| o.id == output_id)?;
    let placed = arrange(state, output_id).placement(key)?;
    Some(Rect {
        x: placed.x + output.geometry.x,
        y: placed.y + output.geometry.y,
        ..placed
    })
}

/// Every layer surface on an output, in a stable order.
///
/// Sorted by layer so the bands stack correctly, and by object id within a
/// band so two surfaces on the same layer keep their order between frames —
/// a `HashMap`'s iteration order would reshuffle the desktop every time it
/// was drawn.
pub fn ordered_for_output(
    state: &CompositorState,
    output_id: OutputId,
) -> Vec<(ClientObjectId, &LayerSurfaceState)> {
    let mut found: Vec<(ClientObjectId, &LayerSurfaceState)> = state
        .layer_surfaces
        .iter()
        .filter(|(_, layer)| layer.output == Some(output_id))
        .map(|(&key, layer)| (key, layer))
        .collect();
    found.sort_by_key(|(key, layer)| (layer.current.layer, *key));
    found
}

/// The layer surfaces on an output that draw in a given band, bottom to top.
pub fn keys_in_band(
    state: &CompositorState,
    output_id: OutputId,
    above_windows: bool,
) -> Vec<ClientObjectId> {
    ordered_for_output(state, output_id)
        .into_iter()
        .filter(|(_, layer)| layer.current.layer.is_above_windows() == above_windows)
        .map(|(_, layer)| (layer.client_id, layer.wl_surface_id))
        .collect()
}
