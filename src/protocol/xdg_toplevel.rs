//! `xdg_toplevel` protocol handler.
//!
//! A toplevel is the primary window role. The compositor sends configure
//! events (size, states like maximized/fullscreen), and the client can
//! set title, `app_id`, and request state changes.

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::{ClientObjectId, CompositorState, GrabKind, ResizeEdges};
use super::wire_utils::{ArgReader, ArgWriter, build_message};

// Request opcodes
const DESTROY: u16 = 0;
const SET_PARENT: u16 = 1;
const SET_TITLE: u16 = 2;
const SET_APP_ID: u16 = 3;
const SHOW_WINDOW_MENU: u16 = 4;
const MOVE: u16 = 5;
const RESIZE: u16 = 6;
pub const SET_MAX_SIZE: u16 = 7;
pub const SET_MIN_SIZE: u16 = 8;
const SET_MAXIMIZED: u16 = 9;
const UNSET_MAXIMIZED: u16 = 10;
const SET_FULLSCREEN: u16 = 11;
const UNSET_FULLSCREEN: u16 = 12;
const SET_MINIMIZED: u16 = 13;

// Event opcodes
const CONFIGURE: u16 = 0;
const CLOSE: u16 = 1;
const CONFIGURE_BOUNDS: u16 = 2;
const WM_CAPABILITIES: u16 = 3;

/// Apply `set_maximized`, `unset_maximized`, `set_fullscreen` or
/// `unset_fullscreen`.
///
/// All four are requests rather than commands: the compositor decides, and
/// answers with a configure either way. Refusing silently is legal and is what
/// happens when the window is on no output — but a configure still goes out, so
/// a client waiting on one is never left hanging.
///
/// Each argument is `None` for "leave this as it was". The two states are
/// independent: a window made fullscreen while maximized is still maximized
/// underneath, and un-fullscreening it must put it back to filling the output
/// rather than to the size it had before either.
fn set_state(
    state: &mut CompositorState,
    msg: &WaylandRequestWithClientInfo,
    maximized: Option<bool>,
    fullscreen: Option<bool>,
) {
    let key = (msg.client_id, msg.message.object_id);
    let Some(current) = state.xdg_toplevels.get(&key) else {
        return;
    };
    let maximized = maximized.unwrap_or(current.maximized);
    let fullscreen = fullscreen.unwrap_or(current.fullscreen);
    debug!("xdg_toplevel: maximized={maximized} fullscreen={fullscreen} for {key:?}");

    // `set_fullscreen` names an output it would like to be fullscreen on. It is
    // read to keep the argument accounting honest, and ignored: a window is
    // confined to the output its workspace belongs to, so the compositor has
    // already decided which display it is on.
    if !state.set_window_state(key, maximized, fullscreen) {
        // Nothing changed, but the client asked and is owed an answer.
        configure_unchanged(state, key);
    }
}

/// Answer a state request that changed nothing, so the client is not left
/// waiting on a configure that will never come.
fn configure_unchanged(state: &mut CompositorState, key: ClientObjectId) {
    let size = state
        .surface_of_toplevel(key)
        .map_or((0, 0), |surface| state.surface_size(surface));
    configure(state, key, size.0, size.1);
}

/// Record which window a dialog belongs to.
///
/// The parent is kept above nothing and below its children: raising either
/// brings the pair up together, so a dialog cannot end up behind the window it
/// belongs to. A null parent detaches the window, which is how a client
/// promotes a dialog to a window in its own right.
fn handle_set_parent(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(parent_id) = args.u32() else {
        super::reject_malformed_request(state, msg, "xdg_toplevel");
        return;
    };
    let key = (msg.client_id, msg.message.object_id);

    // A parent must be a toplevel of the same client, and may not be the window
    // itself or anything below it — a cycle would make the stacking pass below
    // run forever.
    let parent = if parent_id == 0 {
        None
    } else {
        let candidate = (msg.client_id, parent_id);
        if !state.xdg_toplevels.contains_key(&candidate) || would_cycle(state, key, candidate) {
            debug!("xdg_toplevel.set_parent: {parent_id} is not a usable parent, ignoring");
            return;
        }
        Some(parent_id)
    };

    debug!("xdg_toplevel.set_parent: {key:?} -> {parent:?}");
    if let Some(toplevel) = state.xdg_toplevels.get_mut(&key) {
        toplevel.parent = parent;
    }
    state.raise_with_children(key);
}

/// Whether making `parent` the parent of `child` would close a loop.
fn would_cycle(state: &CompositorState, child: ClientObjectId, parent: ClientObjectId) -> bool {
    let mut seen = 0usize;
    let mut current = Some(parent);
    while let Some(key) = current {
        if key == child {
            return true;
        }
        // The chain is at most as long as the toplevels that exist; anything
        // longer means one is already there and should not be walked further.
        seen += 1;
        if seen > state.xdg_toplevels.len() {
            return true;
        }
        current = state
            .xdg_toplevels
            .get(&key)
            .and_then(|t| t.parent)
            .map(|id| (key.0, id));
    }
    false
}

/// `xdg_toplevel.wm_capability`: the window-management requests a compositor
/// may support.
const WM_CAP_WINDOW_MENU: u32 = 1;
const WM_CAP_MAXIMIZE: u32 = 2;
const WM_CAP_FULLSCREEN: u32 = 3;
const WM_CAP_MINIMIZE: u32 = 4;

/// The version at which `wm_capabilities` appears.
const WM_CAPABILITIES_SINCE: u32 = 5;
/// The version at which `configure_bounds` appears.
const CONFIGURE_BOUNDS_SINCE: u32 = 4;

/// Every capability the protocol defines, and whether this compositor honours
/// the requests behind it.
///
/// A flag here and the arm in [`handle`] behind it are two halves of one claim,
/// and a table is what stops them drifting apart: turning a capability on means
/// flipping one word next to the request that implements it.
const WM_CAPABILITIES_SUPPORTED: &[(u32, bool)] = &[
    // No menus: the compositor has no text rendering and no widgets, so a
    // client asking for one would be shown nothing.
    (WM_CAP_WINDOW_MENU, false),
    (WM_CAP_MAXIMIZE, true),
    (WM_CAP_FULLSCREEN, true),
    // No taskbar, dock or window list, so a minimized window would have no way
    // back onto the screen.
    (WM_CAP_MINIMIZE, false),
];

/// Tell a client how much room it has to size itself into.
///
/// The bounds are the output the window is on. There are no panels or docks to
/// subtract, so the whole display is available — but the point of the event is
/// that the client should not *assume* that, and a window opening larger than
/// the screen it is on is the thing this prevents.
///
/// Sent only when the answer changes, which for a window that stays on one
/// display means once. A window with no output yet is told nothing rather than
/// told zero: zero means "no bounds known" and would have to be corrected.
fn send_configure_bounds(state: &mut CompositorState, key: ClientObjectId) {
    let bounds = state
        .surface_of_toplevel(key)
        .and_then(|surface| state.surface_output(surface))
        .and_then(|id| state.outputs.iter().find(|o| o.id == id))
        .map(|output| (output.logical_size().0, output.logical_size().1));
    let Some((width, height)) = bounds else {
        return;
    };
    if state.xdg_toplevels.get(&key).and_then(|t| t.sent_bounds) == Some((width, height)) {
        return;
    }
    if let Some(toplevel) = state.xdg_toplevels.get_mut(&key) {
        toplevel.sent_bounds = Some((width, height));
    }

    let args = ArgWriter::new().i32(width).i32(height).build();
    if let Some(client) = state.clients.get(key.0)
        && client.version(key.1) >= CONFIGURE_BOUNDS_SINCE
    {
        let _ = client.send(build_message(key.1, CONFIGURE_BOUNDS, args));
    }
}

/// Tell a client which window-management requests actually do anything.
///
/// This has to be sent, and sent empty when that is the truth. A client told
/// nothing must assume *every* capability is available, so silence is not
/// neutral — it is a claim that maximize, fullscreen, minimize and the window
/// menu all work. Toolkits act on it by drawing the title-bar buttons, and each
/// one then does nothing when pressed.
pub fn send_wm_capabilities(state: &mut CompositorState, client_id: u32, toplevel_id: u32) {
    let capabilities: Vec<u32> = WM_CAPABILITIES_SUPPORTED
        .iter()
        .filter(|(_, supported)| *supported)
        .map(|&(capability, _)| capability)
        .collect();

    let args = ArgWriter::new().array_u32(&capabilities).build();
    if let Some(client) = state.clients.get(client_id)
        && client.version(toplevel_id) >= WM_CAPABILITIES_SINCE
    {
        let _ = client.send(build_message(toplevel_id, WM_CAPABILITIES, args));
    }
}

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        DESTROY => handle_destroy(state, msg),
        SET_TITLE => handle_set_title(state, msg),
        SET_APP_ID => handle_set_app_id(state, msg),
        MOVE => handle_move(state, msg),
        RESIZE => handle_resize(state, msg),
        SET_PARENT => handle_set_parent(state, msg),
        SHOW_WINDOW_MENU => {
            // The compositor draws no menus — it has no text rendering and no
            // widgets — so there is nothing to show. `wm_capabilities` reports
            // the capability as absent, which is a client's cue to draw its own
            // menu rather than ask for one.
            debug!("xdg_toplevel.show_window_menu: no compositor menu to show");
        }
        SET_MIN_SIZE => handle_set_size_hint(state, msg, SizeHint::Min),
        SET_MAX_SIZE => handle_set_size_hint(state, msg, SizeHint::Max),
        SET_MAXIMIZED => set_state(state, msg, Some(true), None),
        UNSET_MAXIMIZED => set_state(state, msg, Some(false), None),
        SET_FULLSCREEN => set_state(state, msg, None, Some(true)),
        UNSET_FULLSCREEN => set_state(state, msg, None, Some(false)),
        SET_MINIMIZED => {
            // Not implemented, and `wm_capabilities` says so, so a client that
            // reads it will not offer the button. There is no taskbar, dock or
            // window list in this compositor, so a minimized window would have
            // no way back to the screen — hiding one would lose it.
            debug!("xdg_toplevel.set_minimized: not supported");
        }
        _ => super::reject_unknown_request(state, msg, "xdg_toplevel"),
    }
}

fn handle_destroy(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let toplevel_id = msg.message.object_id;
    debug!("xdg_toplevel.destroy: toplevel_id={}", toplevel_id);
    state.destroy_xdg_toplevel(msg.client_id, toplevel_id);
    if let Some(client) = state.clients.get(msg.client_id) {
        client.unregister(toplevel_id);
    }
}

fn handle_set_title(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(title) = args.string() else {
        super::reject_malformed_request(state, msg, "xdg_toplevel");
        return;
    };
    let toplevel_id = msg.message.object_id;
    debug!("xdg_toplevel.set_title: \"{}\"", title);
    if let Some(toplevel) = state.xdg_toplevels.get_mut(&(msg.client_id, toplevel_id)) {
        toplevel.title = Some(title);
    }
}

fn handle_set_app_id(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(app_id) = args.string() else {
        super::reject_malformed_request(state, msg, "xdg_toplevel");
        return;
    };
    let toplevel_id = msg.message.object_id;
    debug!("xdg_toplevel.set_app_id: \"{}\"", app_id);
    if let Some(toplevel) = state.xdg_toplevels.get_mut(&(msg.client_id, toplevel_id)) {
        toplevel.app_id = Some(app_id);
    }
}

// `xdg_toplevel` state values
/// Which end of a client's acceptable size range a request is setting.
#[derive(Debug, Clone, Copy)]
enum SizeHint {
    Min,
    Max,
}

/// Record a client's `set_min_size` or `set_max_size`.
///
/// Applied straight away rather than on the next commit as the protocol
/// specifies. The difference is only visible to a client that sets a limit and
/// starts a resize before committing, and the compositor keeps no other
/// double-buffered toplevel state to hang this off.
///
/// A min larger than a max is not treated as an error here. The two arrive in
/// separate requests, so a client raising both would momentarily look
/// inconsistent through no fault of its own; the resize path resolves the
/// overlap instead.
fn handle_set_size_hint(
    state: &mut CompositorState,
    msg: &WaylandRequestWithClientInfo,
    hint: SizeHint,
) {
    let mut args = ArgReader::new(&msg.message.args);
    let (Some(width), Some(height)) = (args.i32(), args.i32()) else {
        super::reject_malformed_request(state, msg, "xdg_toplevel");
        return;
    };
    let toplevel_id = msg.message.object_id;

    if width < 0 || height < 0 {
        if let Some(client) = state.clients.get(msg.client_id) {
            // XDG_TOPLEVEL_ERROR_INVALID_SIZE = 2
            client.send_error(
                toplevel_id,
                2,
                "xdg_toplevel: size hint must not be negative",
            );
        }
        return;
    }

    debug!("xdg_toplevel.set_{hint:?}_size: {width}x{height}");
    if let Some(toplevel) = state.xdg_toplevels.get_mut(&(msg.client_id, toplevel_id)) {
        match hint {
            SizeHint::Min => toplevel.min_size = (width, height),
            SizeHint::Max => toplevel.max_size = (width, height),
        }
    }
}

/// Start an interactive move, if the client is entitled to.
fn handle_move(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    // move args: object seat, uint serial
    let (Some(_seat), Some(serial)) = (args.u32(), args.u32()) else {
        super::reject_malformed_request(state, msg, "xdg_toplevel");
        return;
    };
    let Some(surface) = grab_target(state, msg.client_id, msg.message.object_id, serial) else {
        return;
    };
    debug!("xdg_toplevel.move: toplevel_id={}", msg.message.object_id);
    state.start_move_grab(surface);
}

/// Start an interactive resize, if the client is entitled to.
fn handle_resize(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    // resize args: object seat, uint serial, uint edges
    let (Some(_seat), Some(serial), Some(edges)) = (args.u32(), args.u32(), args.u32()) else {
        super::reject_malformed_request(state, msg, "xdg_toplevel");
        return;
    };
    let Some(surface) = grab_target(state, msg.client_id, msg.message.object_id, serial) else {
        return;
    };
    debug!(
        "xdg_toplevel.resize: toplevel_id={} edges={}",
        msg.message.object_id, edges
    );
    state.start_resize_grab(surface, ResizeEdges(edges));
}

/// Resolve a grab request to the window it should act on.
///
/// A client may only start a grab off the back of real user input, or any
/// client could seize the pointer whenever it liked. The serial it quotes must
/// be one we minted for a button press, and that button must still be down —
/// a grab beginning after the user let go has nothing to follow.
pub fn grab_target(
    state: &CompositorState,
    client_id: u32,
    toplevel_id: u32,
    serial: u32,
) -> Option<ClientObjectId> {
    if state.pressed_buttons.is_empty() {
        debug!("xdg_toplevel grab refused: no button is held");
        return None;
    }
    if state.last_button_serial.get(&client_id) != Some(&serial) {
        debug!("xdg_toplevel grab refused: serial {serial} was not a recent press");
        return None;
    }
    let xdg_surface_id = state
        .xdg_toplevels
        .get(&(client_id, toplevel_id))?
        .xdg_surface_id;
    let wl_surface_id = state
        .xdg_surfaces
        .get(&(client_id, xdg_surface_id))?
        .wl_surface_id;
    Some((client_id, wl_surface_id))
}

/// `xdg_toplevel.state` values.
const STATE_MAXIMIZED: u32 = 1;
const STATE_FULLSCREEN: u32 = 2;
const STATE_RESIZING: u32 = 3;
const STATE_ACTIVATED: u32 = 4;

/// Every state a toplevel is currently in.
///
/// Built in one place and from compositor state rather than assembled at each
/// call site, because a configure carries the *complete* set — a client reading
/// one that omits `maximized` is being told the window is no longer maximized.
/// The previous shape, where activation and resizing each wrote their own
/// two-element array, could only ever have said one thing at a time.
fn current_states(state: &CompositorState, key: ClientObjectId) -> Vec<u32> {
    let mut states = Vec::new();
    let Some(toplevel) = state.xdg_toplevels.get(&key) else {
        return states;
    };
    if toplevel.maximized {
        states.push(STATE_MAXIMIZED);
    }
    if toplevel.fullscreen {
        states.push(STATE_FULLSCREEN);
    }
    if state
        .pointer_grab
        .is_some_and(|grab| grab.toplevel == key && matches!(grab.kind, GrabKind::Resize { .. }))
    {
        states.push(STATE_RESIZING);
    }
    if state
        .xdg_surfaces
        .get(&(key.0, toplevel.xdg_surface_id))
        .is_some_and(|xdg| state.focused_surface == Some((key.0, xdg.wl_surface_id)))
    {
        states.push(STATE_ACTIVATED);
    }
    states
}

/// Ask a client to adopt a size, telling it every state it is in, and pair the
/// request with the `xdg_surface.configure` that makes it take effect.
///
/// The two always travel together: a size only applies once the client
/// acknowledges the serial, so a toplevel configure sent without one is a
/// request the client is entitled to ignore forever.
pub fn configure(state: &mut CompositorState, key: ClientObjectId, width: i32, height: i32) {
    // Before the configure, which is where the protocol puts it: a client sizes
    // itself against the bounds, so hearing them afterwards is too late.
    send_configure_bounds(state, key);

    let states = current_states(state, key);
    send_configure_with_states(state, key.0, key.1, width, height, &states);

    let Some(xdg_surface_id) = state.xdg_toplevels.get(&key).map(|t| t.xdg_surface_id) else {
        return;
    };
    super::xdg_surface::send_configure(state, key.0, xdg_surface_id);
}

/// Ask a client to adopt a size during or at the end of an interactive resize.
///
/// The `resizing` state tells the client the drag is still in progress, which
/// is its cue to favour speed over quality — skipping reflow it would only
/// have to redo on the next motion event.
pub fn send_resize_configure(
    state: &mut CompositorState,
    client_id: u32,
    toplevel_id: u32,
    width: i32,
    height: i32,
    resizing: bool,
) {
    // `resizing` is read off the live grab by `current_states`, so the flag
    // here only decides whether the grab has already been cleared by the time
    // we are called — which is what the end of a drag looks like.
    let mut states = current_states(state, (client_id, toplevel_id));
    if resizing && !states.contains(&STATE_RESIZING) {
        states.push(STATE_RESIZING);
    }
    send_configure_with_states(state, client_id, toplevel_id, width, height, &states);
}

/// Send `xdg_toplevel`.configure with explicit states.
fn send_configure_with_states(
    state: &mut CompositorState,
    client_id: u32,
    toplevel_id: u32,
    width: i32,
    height: i32,
    states: &[u32],
) {
    // Build args: i32 width, i32 height, wl_array(states)
    // wl_array = u32 byte-length + raw u32 values (already 4-byte aligned)
    let mut args = ArgWriter::new()
        .i32(width)
        .i32(height)
        .u32(u32::try_from(states.len() * 4).expect("States length should be < u32::MAX"));
    for &s in states {
        args = args.u32(s);
    }
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(toplevel_id, CONFIGURE, args.build()));
    }
}

/// Find the `xdg_surface` and `xdg_toplevel` object ids backing a client's
/// `wl_surface`, if that surface has the toplevel role.
///
/// Most compositor-side state is keyed by `wl_surface`, but the `xdg_shell`
/// events are addressed to the toplevel, so this bridges the two.
pub fn xdg_ids_for_surface(
    state: &CompositorState,
    client_id: u32,
    wl_surface_id: u32,
) -> Option<(u32, u32)> {
    state.xdg_surfaces.iter().find_map(|(key, xdg_surface)| {
        if key.0 != client_id || xdg_surface.wl_surface_id != wl_surface_id {
            return None;
        }
        match &xdg_surface.role {
            Some(super::super::state::XdgRole::Toplevel(tid)) => Some((key.1, *tid)),
            _ => None,
        }
    })
}

/// Send a configure sequence to set or clear the activated state on a toplevel.
/// Looks up the toplevel from a `wl_surface` id and sends toplevel.configure +
/// `xdg_surface.configure(serial)`.
pub fn send_activated(
    state: &mut CompositorState,
    client_id: u32,
    wl_surface_id: u32,
    activated: bool,
) {
    let client = state.clients.get(client_id);
    if client.is_none() {
        tracing::warn!("Received message from unknown client {}", client_id);
        return;
    }
    let Some((xdg_surface_id, toplevel_id)) = xdg_ids_for_surface(state, client_id, wl_surface_id)
    else {
        return;
    };

    // `activated` is read from `focused_surface` by `current_states`, which the
    // caller has already updated — the flag is only here to catch the leave
    // half, where focus has moved on but this window has not been told.
    let mut states = current_states(state, (client_id, toplevel_id));
    if !activated {
        states.retain(|&s| s != STATE_ACTIVATED);
    } else if !states.contains(&STATE_ACTIVATED) {
        states.push(STATE_ACTIVATED);
    }
    send_configure_with_states(state, client_id, toplevel_id, 0, 0, &states);

    // And the xdg_surface.configure whose serial the client must acknowledge.
    super::xdg_surface::send_configure(state, client_id, xdg_surface_id);
}

/// Send `xdg_toplevel.close`, asking the client to close the window.
///
/// This is a request, not a command: the client decides what to do, and may
/// prompt the user or ignore it entirely. A client that agrees destroys the
/// toplevel, which tears the window down through the normal destroy path.
pub fn send_close(state: &mut CompositorState, client_id: u32, toplevel_id: u32) {
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(toplevel_id, CLOSE, Vec::new()));
    }
}
