//! `xdg_positioner` protocol handler.
//!
//! A positioner describes how a popup surface should be placed relative to
//! its parent. Clients set size, anchor rect, anchor edge, gravity, offset,
//! and constraint adjustments before passing the positioner to `get_popup`.

use enumflags2::BitFlags;
use tracing::debug;

use super::super::state::{
    XdgPositionerAnchor, XdgPositionerConstraintAdjustment, XdgPositionerGravity,
    XdgPositionerState,
};
use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::CompositorState;
use super::wire_utils::ArgReader;

// Request opcodes
const DESTROY: u16 = 0;
const SET_SIZE: u16 = 1;
const SET_ANCHOR_RECT: u16 = 2;
const SET_ANCHOR: u16 = 3;
const SET_GRAVITY: u16 = 4;
const SET_CONSTRAINT_ADJUSTMENT: u16 = 5;
const SET_OFFSET: u16 = 6;
// Since version 3. Advertised — xdg_wm_base goes out at version 5 — so they
// must be in the match even though nothing acts on them yet; an opcode a
// client is entitled to send has to be told apart from one that does not
// exist. Popups here are positioned once and not repositioned, so a reactive
// popup simply stays where it was put.
const SET_REACTIVE: u16 = 7;
const SET_PARENT_SIZE: u16 = 8;
const SET_PARENT_CONFIGURE: u16 = 9;

/// `xdg_positioner.error.invalid_input`: an argument outside what the enum or
/// the request allows.
const ERROR_INVALID_INPUT: u32 = 0;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        DESTROY => handle_destroy(state, msg),
        SET_SIZE => handle_set_size(state, msg),
        SET_ANCHOR_RECT => handle_set_anchor_rect(state, msg),
        SET_ANCHOR => handle_set_anchor(state, msg),
        SET_GRAVITY => handle_set_gravity(state, msg),
        SET_CONSTRAINT_ADJUSTMENT => handle_set_constraint_adjustment(state, msg),
        SET_OFFSET => handle_set_offset(state, msg),
        SET_REACTIVE => {
            // No arguments: asking at all is the request.
            if let Some(pos) = state
                .xdg_positioners
                .get_mut(&(msg.client_id, msg.message.object_id))
            {
                pos.reactive = true;
            }
        }
        SET_PARENT_SIZE => handle_set_parent_size(state, msg),
        SET_PARENT_CONFIGURE => {
            // The serial of a parent configure this positioner is meant to go
            // with. Nothing is stored: it would only matter if popup placement
            // were deferred until the parent acknowledged that configure, and
            // placement here happens immediately against the geometry the
            // compositor already has.
            debug!("xdg_positioner.set_parent_configure: placement is not deferred");
        }
        _ => super::reject_unknown_request(state, msg, "xdg_positioner"),
    }
}

fn handle_destroy(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let id = msg.message.object_id;
    state.destroy_xdg_positioner(msg.client_id, id);
    if let Some(client) = state.clients.get(msg.client_id) {
        client.unregister(id);
    } else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
    }
}

fn handle_set_size(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let (Some(width), Some(height)) = (args.i32(), args.i32()) else {
        super::reject_malformed_request(state, msg, "xdg_positioner");
        return;
    };
    let id = msg.message.object_id;
    if let Some(p) = state.xdg_positioners.get_mut(&(msg.client_id, id)) {
        p.width = width;
        p.height = height;
    }
}

fn handle_set_anchor_rect(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let (Some(x), Some(y), Some(w), Some(h)) = (args.i32(), args.i32(), args.i32(), args.i32())
    else {
        super::reject_malformed_request(state, msg, "xdg_positioner");
        return;
    };
    let id = msg.message.object_id;
    if let Some(p) = state.xdg_positioners.get_mut(&(msg.client_id, id)) {
        p.anchor_rect = (x, y, w, h);
    }
}

fn handle_set_anchor(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(anchor) = args.u32() else {
        super::reject_malformed_request(state, msg, "xdg_positioner");
        return;
    };
    let id = msg.message.object_id;
    // Decoded before the positioner is borrowed, and refused rather than
    // unwrapped: `anchor` is a client-supplied `u32` and only nine values name
    // an anchor. `invalid_input` is what the protocol has for exactly this,
    // and it costs the offending client its connection rather than costing
    // every client the compositor.
    let Some(anchor) = XdgPositionerAnchor::from_repr(anchor) else {
        if let Some(client) = state.clients.get(msg.client_id) {
            client.send_error(
                id,
                ERROR_INVALID_INPUT,
                "xdg_positioner.set_anchor: not an anchor",
            );
        }
        return;
    };
    if let Some(p) = state.xdg_positioners.get_mut(&(msg.client_id, id)) {
        p.anchor = anchor;
    }
}

fn handle_set_gravity(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(gravity) = args.u32() else {
        super::reject_malformed_request(state, msg, "xdg_positioner");
        return;
    };
    let id = msg.message.object_id;
    // Refused rather than unwrapped, for the reason `set_anchor` gives.
    let Some(gravity) = XdgPositionerGravity::from_repr(gravity) else {
        if let Some(client) = state.clients.get(msg.client_id) {
            client.send_error(
                id,
                ERROR_INVALID_INPUT,
                "xdg_positioner.set_gravity: not a gravity",
            );
        }
        return;
    };
    if let Some(p) = state.xdg_positioners.get_mut(&(msg.client_id, id)) {
        p.gravity = gravity;
    }
}

fn handle_set_constraint_adjustment(
    state: &mut CompositorState,
    msg: &WaylandRequestWithClientInfo,
) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(adjustment) = args.u32() else {
        super::reject_malformed_request(state, msg, "xdg_positioner");
        return;
    };
    let id = msg.message.object_id;
    if let Some(p) = state.xdg_positioners.get_mut(&(msg.client_id, id)) {
        p.constraint_adjustment = BitFlags::from_bits_truncate(adjustment);
    }
}

fn handle_set_offset(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let (Some(x), Some(y)) = (args.i32(), args.i32()) else {
        super::reject_malformed_request(state, msg, "xdg_positioner");
        return;
    };
    let id = msg.message.object_id;
    if let Some(p) = state.xdg_positioners.get_mut(&(msg.client_id, id)) {
        p.offset = (x, y);
    }
}

fn handle_set_parent_size(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let (Some(width), Some(height)) = (args.i32(), args.i32()) else {
        super::reject_malformed_request(state, msg, "xdg_positioner");
        return;
    };
    if let Some(pos) = state
        .xdg_positioners
        .get_mut(&(msg.client_id, msg.message.object_id))
    {
        // Zero means the client is withdrawing the hint, not claiming a
        // zero-sized parent — constraining against that would put every popup
        // in the same corner.
        pos.parent_size = (width > 0 && height > 0).then_some((width, height));
    }
}

/// Where a popup goes, in coordinates relative to its parent surface.
///
/// Two steps, and the second is the one that matters on a real desktop. First
/// the anchor and gravity say where the client *wants* it: a point on the
/// anchor rectangle, and which corner of the popup to hang off that point.
/// Then the constraint adjustment says what to do when that lands off-screen,
/// which is the ordinary case for a menu opened near an edge.
///
/// `available` is the region the popup should stay inside, in the same
/// parent-relative coordinates — the output the parent is on, less the parent's
/// own origin. `None` means nothing to constrain against, and the client's
/// first choice is used unaltered.
pub fn place(
    pos: &XdgPositionerState,
    available: Option<(i32, i32, i32, i32)>,
) -> (i32, i32, i32, i32) {
    let (mut x, mut y) = unconstrained(pos, pos.anchor, pos.gravity);
    let (mut width, mut height) = (pos.width, pos.height);

    let Some((ax, ay, aw, ah)) = available else {
        return (x, y, width, height);
    };
    let adjust = pos.constraint_adjustment;

    // Each axis is settled on its own. The protocol treats them separately, and
    // a menu that runs off the right edge should slide sideways without also
    // being dragged upwards.
    if adjust.contains(XdgPositionerConstraintAdjustment::FlipX)
        && !fits(x, width, ax, aw)
        && let (flipped, _) = unconstrained(pos, flip_x(pos.anchor), flip_x_gravity(pos.gravity))
        && fits(flipped, width, ax, aw)
    {
        // Only taken if flipping actually helps: the protocol says an
        // adjustment that would still not fit must be discarded, so a popup
        // near neither edge is not thrown to the other side for nothing.
        x = flipped;
    }
    if adjust.contains(XdgPositionerConstraintAdjustment::FlipY)
        && !fits(y, height, ay, ah)
        && let (_, flipped) = unconstrained(pos, flip_y(pos.anchor), flip_y_gravity(pos.gravity))
        && fits(flipped, height, ay, ah)
    {
        y = flipped;
    }
    if adjust.contains(XdgPositionerConstraintAdjustment::SlideX) && !fits(x, width, ax, aw) {
        x = slide(x, width, ax, aw);
    }
    if adjust.contains(XdgPositionerConstraintAdjustment::SlideY) && !fits(y, height, ay, ah) {
        y = slide(y, height, ay, ah);
    }
    if adjust.contains(XdgPositionerConstraintAdjustment::ResizeX) && !fits(x, width, ax, aw) {
        let (nx, nw) = shrink(x, width, ax, aw);
        x = nx;
        width = nw;
    }
    if adjust.contains(XdgPositionerConstraintAdjustment::ResizeY) && !fits(y, height, ay, ah) {
        let (ny, nh) = shrink(y, height, ay, ah);
        y = ny;
        height = nh;
    }

    (x, y, width, height)
}

/// Where the client's anchor and gravity put the popup, before any constraint.
fn unconstrained(
    pos: &XdgPositionerState,
    anchor: XdgPositionerAnchor,
    gravity: XdgPositionerGravity,
) -> (i32, i32) {
    let (ar_x, ar_y, ar_w, ar_h) = pos.anchor_rect;

    let anchor_x = match anchor {
        XdgPositionerAnchor::Left
        | XdgPositionerAnchor::TopLeft
        | XdgPositionerAnchor::BottomLeft => ar_x,
        XdgPositionerAnchor::Right
        | XdgPositionerAnchor::TopRight
        | XdgPositionerAnchor::BottomRight => ar_x.saturating_add(ar_w),
        _ => ar_x.saturating_add(ar_w / 2),
    };
    let anchor_y = match anchor {
        XdgPositionerAnchor::Top | XdgPositionerAnchor::TopLeft | XdgPositionerAnchor::TopRight => {
            ar_y
        }
        XdgPositionerAnchor::Bottom
        | XdgPositionerAnchor::BottomLeft
        | XdgPositionerAnchor::BottomRight => ar_y.saturating_add(ar_h),
        _ => ar_y.saturating_add(ar_h / 2),
    };

    // Gravity names the direction the popup extends *away* from the anchor, so
    // a gravity of `left` puts the popup's right edge on the anchor point.
    let x = match gravity {
        XdgPositionerGravity::Left
        | XdgPositionerGravity::TopLeft
        | XdgPositionerGravity::BottomLeft => anchor_x.saturating_sub(pos.width),
        XdgPositionerGravity::Right
        | XdgPositionerGravity::TopRight
        | XdgPositionerGravity::BottomRight => anchor_x,
        _ => anchor_x.saturating_sub(pos.width / 2),
    };
    let y = match gravity {
        XdgPositionerGravity::Top
        | XdgPositionerGravity::TopLeft
        | XdgPositionerGravity::TopRight => anchor_y.saturating_sub(pos.height),
        XdgPositionerGravity::Bottom
        | XdgPositionerGravity::BottomLeft
        | XdgPositionerGravity::BottomRight => anchor_y,
        _ => anchor_y.saturating_sub(pos.height / 2),
    };

    (
        x.saturating_add(pos.offset.0),
        y.saturating_add(pos.offset.1),
    )
}

/// Whether a span of `size` starting at `start` lies inside `[origin, origin + extent)`.
fn fits(start: i32, size: i32, origin: i32, extent: i32) -> bool {
    start >= origin && start.saturating_add(size) <= origin.saturating_add(extent)
}

/// Move a span back inside, keeping its far edge in if it is too big to fit.
fn slide(start: i32, size: i32, origin: i32, extent: i32) -> i32 {
    let limit = origin.saturating_add(extent).saturating_sub(size);
    // `max(origin)` last, so a popup larger than the space is pinned to the
    // near edge rather than pushed off the near side to fit the far one.
    start.min(limit).max(origin)
}

/// Cut a span down to what will fit.
fn shrink(start: i32, size: i32, origin: i32, extent: i32) -> (i32, i32) {
    let start = start.max(origin);
    let limit = origin.saturating_add(extent);
    (start, size.min(limit.saturating_sub(start)).max(1))
}

fn flip_x(anchor: XdgPositionerAnchor) -> XdgPositionerAnchor {
    use XdgPositionerAnchor as A;
    match anchor {
        A::Left => A::Right,
        A::Right => A::Left,
        A::TopLeft => A::TopRight,
        A::TopRight => A::TopLeft,
        A::BottomLeft => A::BottomRight,
        A::BottomRight => A::BottomLeft,
        other => other,
    }
}

fn flip_y(anchor: XdgPositionerAnchor) -> XdgPositionerAnchor {
    use XdgPositionerAnchor as A;
    match anchor {
        A::Top => A::Bottom,
        A::Bottom => A::Top,
        A::TopLeft => A::BottomLeft,
        A::BottomLeft => A::TopLeft,
        A::TopRight => A::BottomRight,
        A::BottomRight => A::TopRight,
        other => other,
    }
}

fn flip_x_gravity(gravity: XdgPositionerGravity) -> XdgPositionerGravity {
    use XdgPositionerGravity as G;
    match gravity {
        G::Left => G::Right,
        G::Right => G::Left,
        G::TopLeft => G::TopRight,
        G::TopRight => G::TopLeft,
        G::BottomLeft => G::BottomRight,
        G::BottomRight => G::BottomLeft,
        other => other,
    }
}

fn flip_y_gravity(gravity: XdgPositionerGravity) -> XdgPositionerGravity {
    use XdgPositionerGravity as G;
    match gravity {
        G::Top => G::Bottom,
        G::Bottom => G::Top,
        G::TopLeft => G::BottomLeft,
        G::BottomLeft => G::TopLeft,
        G::TopRight => G::BottomRight,
        G::BottomRight => G::TopRight,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::XdgPositionerState;

    /// A 20x20 popup hung off the bottom-left of a 10x10 anchor rect at (100, 100).
    fn positioner() -> XdgPositionerState {
        XdgPositionerState {
            client_id: 1,
            width: 20,
            height: 20,
            anchor_rect: (100, 100, 10, 10),
            anchor: XdgPositionerAnchor::BottomLeft,
            gravity: XdgPositionerGravity::BottomRight,
            offset: (0, 0),
            constraint_adjustment: BitFlags::empty(),
            reactive: false,
            parent_size: None,
        }
    }

    #[test]
    fn anchor_and_gravity_decide_where_it_goes() {
        // Bottom-left of the anchor rect is (100, 110); gravity bottom-right
        // extends down and right from there.
        assert_eq!(place(&positioner(), None), (100, 110, 20, 20));
    }

    #[test]
    fn an_unconstrained_popup_is_left_where_the_client_asked() {
        // It runs off a narrow screen, but the client asked for no adjustment,
        // so the compositor does not second-guess it.
        let available = Some((0, 0, 105, 200));
        assert_eq!(place(&positioner(), available), (100, 110, 20, 20));
    }

    #[test]
    fn sliding_brings_a_popup_back_onto_the_screen() {
        let mut pos = positioner();
        pos.constraint_adjustment = XdgPositionerConstraintAdjustment::SlideX.into();
        // 105 wide, popup is 20: the furthest left edge that fits is 85.
        assert_eq!(place(&pos, Some((0, 0, 105, 200))), (85, 110, 20, 20));
    }

    #[test]
    fn flipping_puts_the_popup_on_the_other_side_of_its_anchor() {
        let mut pos = positioner();
        pos.constraint_adjustment = XdgPositionerConstraintAdjustment::FlipX.into();
        // Flipped, it hangs off the right edge of the anchor rect extending
        // left, which fits.
        assert_eq!(place(&pos, Some((0, 0, 115, 200))), (90, 110, 20, 20));
    }

    #[test]
    fn a_flip_that_would_not_help_is_discarded() {
        let mut pos = positioner();
        pos.constraint_adjustment = XdgPositionerConstraintAdjustment::FlipX.into();
        // Too narrow either way round, so the popup is left where the client
        // asked rather than thrown to the other side for nothing.
        assert_eq!(place(&pos, Some((0, 0, 15, 200))), (100, 110, 20, 20));
    }

    #[test]
    fn resizing_cuts_the_popup_down_to_what_fits() {
        let mut pos = positioner();
        pos.constraint_adjustment = XdgPositionerConstraintAdjustment::ResizeX.into();
        assert_eq!(place(&pos, Some((0, 0, 110, 200))), (100, 110, 10, 20));
    }

    #[test]
    fn a_popup_larger_than_the_space_is_pinned_to_the_near_edge() {
        let mut pos = positioner();
        pos.constraint_adjustment = XdgPositionerConstraintAdjustment::SlideX.into();
        // Nowhere for it to fit; the near edge is the part worth showing.
        assert_eq!(place(&pos, Some((0, 0, 10, 200))).0, 0);
    }

    #[test]
    fn the_axes_are_settled_independently() {
        let mut pos = positioner();
        pos.constraint_adjustment =
            XdgPositionerConstraintAdjustment::SlideX | XdgPositionerConstraintAdjustment::SlideY;
        // Only x is short, so only x moves.
        let (x, y, _, _) = place(&pos, Some((0, 0, 105, 400)));
        assert_eq!((x, y), (85, 110));
    }
}
