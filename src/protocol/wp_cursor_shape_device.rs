//! `wp_cursor_shape_device_v1` protocol handler.
//!
//! Lets a client name a cursor — "text", "grab", "ne-resize" — instead of
//! drawing one and attaching it as a surface.
//!
//! That is worth having for a reason beyond convenience. A client drawing its
//! own cursor has to load a theme, pick a size for the output the pointer is
//! over, and redraw when either changes; every toolkit does this slightly
//! differently, and a client that gets it wrong shows a cursor the wrong size or
//! the wrong theme. Naming a shape moves all of that to the one process that
//! already knows the answers.
//!
//! The shape numbers here are the protocol's, and the names they map to are
//! X11's, because that is what an xcursor theme on disk contains.

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::CompositorState;
use super::wire_utils::ArgReader;

pub const INTERFACE: &str = "wp_cursor_shape_device_v1";

// Request opcodes
const DESTROY: u16 = 0;
const SET_SHAPE: u16 = 1;

/// `wp_cursor_shape_device_v1.error.invalid_shape`.
const ERROR_INVALID_SHAPE: u32 = 1;

/// The xcursor names for each shape in `wp_cursor_shape_device_v1.shape`, in
/// protocol order from 1.
///
/// Each entry is a list because themes disagree about what a cursor is called.
/// The modern names (`default`, `text`, `ns-resize`) come from the CSS-derived
/// set that freedesktop cursor themes have used for some years; the older X11
/// names (`left_ptr`, `xterm`, `sb_v_double_arrow`) are what many themes still
/// ship, sometimes only as symlinks and sometimes not at all. They are tried in
/// order, so a theme with either kind works and a theme with both gets the
/// newer one.
///
/// Version 1 of the interface stops at 34. Two more shapes — `dnd-ask` and
/// `all-resize` — arrived in version 2, which is why this advertises version 1:
/// a shape past the end of this table is `invalid_shape`, and it is better to
/// say plainly that they are not supported than to accept them and show
/// something else.
const SHAPE_NAMES: &[&[&str]] = &[
    /* 1 default */ &["default", "left_ptr"],
    /* 2 context_menu */ &["context-menu", "left_ptr"],
    /* 3 help */ &["help", "question_arrow", "left_ptr"],
    /* 4 pointer */ &["pointer", "hand2", "hand1"],
    /* 5 progress */ &["progress", "left_ptr_watch", "watch"],
    /* 6 wait */ &["wait", "watch"],
    /* 7 cell */ &["cell", "plus", "crosshair"],
    /* 8 crosshair */ &["crosshair", "cross"],
    /* 9 text */ &["text", "xterm"],
    /* 10 vertical_text */ &["vertical-text"],
    /* 11 alias */ &["alias", "dnd-link"],
    /* 12 copy */ &["copy", "dnd-copy"],
    /* 13 move */ &["move", "dnd-move"],
    /* 14 no_drop */ &["no-drop", "dnd-none"],
    /* 15 not_allowed */ &["not-allowed", "crossed_circle"],
    /* 16 grab */ &["grab", "openhand", "hand1"],
    /* 17 grabbing */ &["grabbing", "closedhand", "dnd-none"],
    /* 18 e_resize */ &["e-resize", "right_side", "sb_h_double_arrow"],
    /* 19 n_resize */ &["n-resize", "top_side", "sb_v_double_arrow"],
    /* 20 ne_resize */ &["ne-resize", "top_right_corner"],
    /* 21 nw_resize */ &["nw-resize", "top_left_corner"],
    /* 22 s_resize */ &["s-resize", "bottom_side", "sb_v_double_arrow"],
    /* 23 se_resize */ &["se-resize", "bottom_right_corner"],
    /* 24 sw_resize */ &["sw-resize", "bottom_left_corner"],
    /* 25 w_resize */ &["w-resize", "left_side", "sb_h_double_arrow"],
    /* 26 ew_resize */ &["ew-resize", "sb_h_double_arrow"],
    /* 27 ns_resize */ &["ns-resize", "sb_v_double_arrow"],
    /* 28 nesw_resize */ &["nesw-resize", "fd_double_arrow"],
    /* 29 nwse_resize */ &["nwse-resize", "bd_double_arrow"],
    /* 30 col_resize */ &["col-resize", "sb_h_double_arrow"],
    /* 31 row_resize */ &["row-resize", "sb_v_double_arrow"],
    /* 32 all_scroll */ &["all-scroll", "fleur"],
    /* 33 zoom_in */ &["zoom-in"],
    /* 34 zoom_out */ &["zoom-out"],
];

/// The names to try for a shape, or `None` if the shape is not one this version
/// defines. Shape numbering starts at 1; zero is not a shape.
pub fn names_for(shape: u32) -> Option<&'static [&'static str]> {
    let index = usize::try_from(shape.checked_sub(1)?).ok()?;
    SHAPE_NAMES.get(index).copied()
}

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        DESTROY => {
            if let Some(client) = state.clients.get(msg.client_id) {
                client.unregister(msg.message.object_id);
            }
        }
        SET_SHAPE => handle_set_shape(state, msg),
        _ => super::unknown_request(state, msg, INTERFACE),
    }
}

fn handle_set_shape(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    // set_shape args: uint serial, uint shape
    let (Some(serial), Some(shape)) = (args.u32(), args.u32()) else {
        super::malformed_request(state, msg, INTERFACE);
        return;
    };

    let Some(names) = names_for(shape) else {
        if let Some(client) = state.clients.get(msg.client_id) {
            client.send_error(
                msg.message.object_id,
                ERROR_INVALID_SHAPE,
                &format!("wp_cursor_shape_device_v1.set_shape: {shape} is not a shape"),
            );
        }
        return;
    };

    // The same serial rule as `wl_pointer.set_cursor`, which this request stands
    // in for: it must be the enter the client is answering. A stale one is
    // ignored rather than refused, exactly as there — the client has lost a race
    // it could not see, and the pointer is somewhere else now.
    if state.pointer_enter_serial.get(&msg.client_id) != Some(&serial) {
        debug!("{INTERFACE}.set_shape ignored: serial {serial} is not the current enter");
        return;
    }

    // Loading happens here rather than at draw time: this is a client request
    // arriving on the compositor's own task, so reading a cursor file is the
    // same kind of work as any other request, and doing it while composing a
    // frame would put file IO on the path of every pointer motion. Each shape is
    // read once and kept.
    let loaded = state.ensure_cursor_shape(names);

    // A theme without this shape leaves the cursor as it was rather than
    // replacing it with something wrong. Which shape is missing is worth a log
    // line: it is the theme's doing, not the client's, and nothing else would
    // ever say so.
    if !loaded {
        debug!("{INTERFACE}.set_shape: no cursor in the theme for shape {shape} ({names:?})");
        return;
    }

    // The two ways of setting a cursor are the same choice made twice, so the
    // later one has to win outright: a client that has set a surface and then
    // names a shape must not keep showing the surface.
    state.cursor_surfaces.remove(&msg.client_id);
    state.cursor_shapes.insert(msg.client_id, names);
    state.cursor_dirty = true;
}
