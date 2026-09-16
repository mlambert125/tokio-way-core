//! What each request looks like on the wire.
//!
//! One table of every request the compositor advertises, in opcode order, with
//! the arguments it carries. It exists because three separate things need the
//! same answer and were each working it out for themselves:
//!
//! - [`super::request_fd_count`], which has to know how many descriptors a
//!   request claims before any handler runs. That used to be a hand-kept list
//!   of three special cases, correct only for as long as somebody remembered
//!   to widen it; it is now derived from the signature, so a new request
//!   carrying a descriptor cannot be added without the count following.
//! - The dispatch soak, which can only build interesting state if it emits
//!   requests that decode. A generator inventing argument bytes at random
//!   spends its whole run in the malformed-arguments arm.
//! - A socket-level test harness, which needs to script real exchanges.
//!
//! The table describes the *protocol*, not what a handler happens to read. A
//! request the compositor accepts and ignores still has its real arguments
//! here, because a client will send them and anything generating requests has
//! to produce them. `xdg_popup.grab` is the clearest case: nothing reads its
//! two arguments, and a generator that therefore sent none would be producing
//! a message no client would.
//!
//! Kept honest by [`crate::tests::compositor::protocol::signature`], which
//! checks every entry against the dispatcher: an opcode in this table must be
//! one its interface accepts, and an opcode its interface accepts must be in
//! this table. A signature that drifts from its handler is worse than no
//! signature at all, so the two are checked against each other rather than
//! both being trusted.

use super::ObjectType;

/// One argument's type on the wire.
///
/// The widths are what [`super::wire_utils::ArgReader`] reads. Everything but
/// `String`, `Array` and `Fd` is exactly four bytes; a string and an array are
/// a length followed by padded bytes, and a descriptor occupies none at all —
/// it travels as ancillary data on the socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgType {
    /// `int`: a signed 32-bit integer.
    Int,
    /// `uint`: an unsigned 32-bit integer.
    Uint,
    /// `fixed`: 24.8 signed fixed point.
    Fixed,
    /// `string`: a length, the bytes, a NUL, and padding to four.
    String,
    /// `object`: the id of an existing object, or zero where the protocol
    /// allows a null one.
    Object,
    /// `new_id`: an id the client is allocating.
    NewId,
    /// `array`: a length and that many bytes, padded to four.
    ///
    /// No request the compositor advertises carries one — only events do,
    /// `wl_keyboard.enter`'s held keys being the example — so nothing in the
    /// tables below names it. It is here because it is part of the wire
    /// format, and a request that grows one should find the type waiting
    /// rather than have to invent it.
    #[allow(dead_code)]
    Array,
    /// `fd`: a descriptor, sent out of band and occupying no wire bytes.
    Fd,
}

/// One request: its name, and the arguments it carries in order.
#[derive(Debug, Clone, Copy)]
pub struct Request {
    /// The request's name in the protocol, for messages a human reads.
    pub name: &'static str,
    /// Its arguments, in the order they appear.
    pub args: &'static [ArgType],
}

use ArgType::{Fd, Fixed, Int, NewId, Object, String as Str, Uint};

/// Shorthand for one entry, so the tables below read as the protocol does.
const fn request(name: &'static str, args: &'static [ArgType]) -> Request {
    Request { name, args }
}

const WL_DISPLAY: &[Request] = &[request("sync", &[NewId]), request("get_registry", &[NewId])];

const WL_REGISTRY: &[Request] = &[request("bind", &[Uint, Str, Uint, NewId])];

// wl_callback has no requests at all: it is a one-shot the compositor answers
// and the client then destroys through wl_display.delete_id.
const WL_CALLBACK: &[Request] = &[];

const WL_COMPOSITOR: &[Request] = &[
    request("create_surface", &[NewId]),
    request("create_region", &[NewId]),
];

const WL_SHM: &[Request] = &[request("create_pool", &[NewId, Fd, Int])];

const WL_SHM_POOL: &[Request] = &[
    request("create_buffer", &[NewId, Int, Int, Int, Int, Uint]),
    request("destroy", &[]),
    request("resize", &[Int]),
];

const WL_BUFFER: &[Request] = &[request("destroy", &[])];

const WL_SURFACE: &[Request] = &[
    request("destroy", &[]),
    request("attach", &[Object, Int, Int]),
    request("damage", &[Int, Int, Int, Int]),
    request("frame", &[NewId]),
    request("set_opaque_region", &[Object]),
    request("set_input_region", &[Object]),
    request("commit", &[]),
    request("set_buffer_transform", &[Int]),
    request("set_buffer_scale", &[Int]),
    request("damage_buffer", &[Int, Int, Int, Int]),
    request("offset", &[Int, Int]),
];

const WL_REGION: &[Request] = &[
    request("destroy", &[]),
    request("add", &[Int, Int, Int, Int]),
    request("subtract", &[Int, Int, Int, Int]),
];

const WL_SEAT: &[Request] = &[
    request("get_pointer", &[NewId]),
    request("get_keyboard", &[NewId]),
    request("get_touch", &[NewId]),
    request("release", &[]),
];

const WL_POINTER: &[Request] = &[
    request("set_cursor", &[Uint, Object, Int, Int]),
    request("release", &[]),
];

const WL_KEYBOARD: &[Request] = &[request("release", &[])];

const WL_TOUCH: &[Request] = &[request("release", &[])];

const WL_OUTPUT: &[Request] = &[request("release", &[])];

const WL_SUBCOMPOSITOR: &[Request] = &[
    request("destroy", &[]),
    request("get_subsurface", &[NewId, Object, Object]),
];

const WL_SUBSURFACE: &[Request] = &[
    request("destroy", &[]),
    request("set_position", &[Int, Int]),
    request("place_above", &[Object]),
    request("place_below", &[Object]),
    request("set_sync", &[]),
    request("set_desync", &[]),
];

const WL_DATA_DEVICE_MANAGER: &[Request] = &[
    request("create_data_source", &[NewId]),
    request("get_data_device", &[NewId, Object]),
];

const WL_DATA_DEVICE: &[Request] = &[
    request("start_drag", &[Object, Object, Object, Uint]),
    request("set_selection", &[Object, Uint]),
    request("release", &[]),
];

const WL_DATA_SOURCE: &[Request] = &[
    request("offer", &[Str]),
    request("destroy", &[]),
    request("set_actions", &[Uint]),
];

const WL_DATA_OFFER: &[Request] = &[
    request("accept", &[Uint, Str]),
    request("receive", &[Str, Fd]),
    request("destroy", &[]),
    request("finish", &[]),
    request("set_actions", &[Uint, Uint]),
];

const ZWP_PRIMARY_SELECTION_DEVICE_MANAGER: &[Request] = &[
    request("create_source", &[NewId]),
    request("get_device", &[NewId, Object]),
    request("destroy", &[]),
];

const ZWP_PRIMARY_SELECTION_DEVICE: &[Request] = &[
    request("set_selection", &[Object, Uint]),
    request("destroy", &[]),
];

const ZWP_PRIMARY_SELECTION_SOURCE: &[Request] =
    &[request("offer", &[Str]), request("destroy", &[])];

const ZWP_PRIMARY_SELECTION_OFFER: &[Request] =
    &[request("receive", &[Str, Fd]), request("destroy", &[])];

const ZWLR_DATA_CONTROL_MANAGER: &[Request] = &[
    request("create_data_source", &[NewId]),
    request("get_data_device", &[NewId, Object]),
    request("destroy", &[]),
];

const ZWLR_DATA_CONTROL_DEVICE: &[Request] = &[
    request("set_selection", &[Object]),
    request("destroy", &[]),
    request("set_primary_selection", &[Object]),
];

const ZWLR_DATA_CONTROL_SOURCE: &[Request] = &[request("offer", &[Str]), request("destroy", &[])];

const ZWLR_DATA_CONTROL_OFFER: &[Request] =
    &[request("receive", &[Str, Fd]), request("destroy", &[])];

const WL_FIXES: &[Request] = &[
    request("destroy", &[]),
    request("destroy_registry", &[Object]),
];

const XDG_WM_BASE: &[Request] = &[
    request("destroy", &[]),
    request("create_positioner", &[NewId]),
    request("get_xdg_surface", &[NewId, Object]),
    request("pong", &[Uint]),
];

const XDG_SURFACE: &[Request] = &[
    request("destroy", &[]),
    request("get_toplevel", &[NewId]),
    request("get_popup", &[NewId, Object, Object]),
    request("set_window_geometry", &[Int, Int, Int, Int]),
    request("ack_configure", &[Uint]),
];

const XDG_TOPLEVEL: &[Request] = &[
    request("destroy", &[]),
    request("set_parent", &[Object]),
    request("set_title", &[Str]),
    request("set_app_id", &[Str]),
    // Accepted and ignored — there are no menus to show — but a client still
    // sends all four arguments.
    request("show_window_menu", &[Object, Uint, Int, Int]),
    request("move", &[Object, Uint]),
    request("resize", &[Object, Uint, Uint]),
    request("set_max_size", &[Int, Int]),
    request("set_min_size", &[Int, Int]),
    request("set_maximized", &[]),
    request("unset_maximized", &[]),
    request("set_fullscreen", &[Object]),
    request("unset_fullscreen", &[]),
    request("set_minimized", &[]),
];

const XDG_POPUP: &[Request] = &[
    request("destroy", &[]),
    // Nothing reads these two, but a client sends them and anything generating
    // requests has to produce them.
    request("grab", &[Object, Uint]),
    request("reposition", &[Object, Uint]),
];

const XDG_POSITIONER: &[Request] = &[
    request("destroy", &[]),
    request("set_size", &[Int, Int]),
    request("set_anchor_rect", &[Int, Int, Int, Int]),
    request("set_anchor", &[Uint]),
    request("set_gravity", &[Uint]),
    request("set_constraint_adjustment", &[Uint]),
    request("set_offset", &[Int, Int]),
    request("set_reactive", &[]),
    request("set_parent_size", &[Int, Int]),
    request("set_parent_configure", &[Uint]),
];

const XDG_SYSTEM_BELL: &[Request] = &[request("destroy", &[]), request("ring", &[Object])];

const WP_VIEWPORTER: &[Request] = &[
    request("destroy", &[]),
    request("get_viewport", &[NewId, Object]),
];

const WP_VIEWPORT: &[Request] = &[
    request("destroy", &[]),
    request("set_source", &[Fixed, Fixed, Fixed, Fixed]),
    request("set_destination", &[Int, Int]),
];

const WP_CURSOR_SHAPE_MANAGER: &[Request] = &[
    request("destroy", &[]),
    request("get_pointer", &[NewId, Object]),
    // Accepted and inert: there is no tablet protocol here for a client to have
    // a `zwp_tablet_tool_v2` from, but the request is in version 1 of the
    // interface and a client is entitled to send it.
    request("get_tablet_tool_v2", &[NewId, Object]),
];

const WP_CURSOR_SHAPE_DEVICE: &[Request] =
    &[request("destroy", &[]), request("set_shape", &[Uint, Uint])];

const WP_FRACTIONAL_SCALE_MANAGER: &[Request] = &[
    // `destroy` leads here, where most managers put it last.
    request("destroy", &[]),
    request("get_fractional_scale", &[NewId, Object]),
];

const WP_FRACTIONAL_SCALE: &[Request] = &[request("destroy", &[])];

const WP_PRESENTATION: &[Request] = &[
    request("destroy", &[]),
    request("feedback", &[Object, NewId]),
];

// wp_presentation_feedback carries no requests: the compositor answers it and
// the object is done.
const WP_PRESENTATION_FEEDBACK: &[Request] = &[];

const ZWP_LINUX_DMABUF: &[Request] = &[request("destroy", &[]), request("create_params", &[NewId])];

const ZWP_LINUX_BUFFER_PARAMS: &[Request] = &[
    request("destroy", &[]),
    // The descriptor comes first here, unlike `wl_shm.create_pool`.
    request("add", &[Fd, Uint, Uint, Uint, Uint, Uint]),
    request("create", &[Int, Int, Uint, Uint]),
    request("create_immed", &[NewId, Int, Int, Uint, Uint]),
];

const ZXDG_DECORATION_MANAGER: &[Request] = &[
    request("destroy", &[]),
    request("get_toplevel_decoration", &[NewId, Object]),
];

const ZWLR_LAYER_SHELL: &[Request] = &[
    request("get_layer_surface", &[NewId, Object, Object, Uint, Str]),
    request("destroy", &[]),
];

const ZWLR_LAYER_SURFACE: &[Request] = &[
    request("set_size", &[Uint, Uint]),
    request("set_anchor", &[Uint]),
    request("set_exclusive_zone", &[Int]),
    request("set_margin", &[Int, Int, Int, Int]),
    request("set_keyboard_interactivity", &[Uint]),
    request("get_popup", &[Object]),
    request("ack_configure", &[Uint]),
    request("destroy", &[]),
    request("set_layer", &[Uint]),
];

const ZXDG_TOPLEVEL_DECORATION: &[Request] = &[
    request("destroy", &[]),
    request("set_mode", &[Uint]),
    request("unset_mode", &[]),
];

/// Every request an interface has, in opcode order.
///
/// Indexing this by opcode is what makes an opcode outside its range
/// recognisable as one the interface does not have — the same judgement
/// [`super::unknown_request`] exists to make.
pub fn requests(object_type: ObjectType) -> &'static [Request] {
    match object_type {
        ObjectType::WlDisplay => WL_DISPLAY,
        ObjectType::WlRegistry => WL_REGISTRY,
        ObjectType::WlCallback => WL_CALLBACK,
        ObjectType::WlCompositor => WL_COMPOSITOR,
        ObjectType::WlShm => WL_SHM,
        ObjectType::WlShmPool => WL_SHM_POOL,
        ObjectType::WlBuffer => WL_BUFFER,
        ObjectType::WlSurface => WL_SURFACE,
        ObjectType::WlRegion => WL_REGION,
        ObjectType::WlSeat => WL_SEAT,
        ObjectType::WlPointer => WL_POINTER,
        ObjectType::WlKeyboard => WL_KEYBOARD,
        ObjectType::WlTouch => WL_TOUCH,
        ObjectType::WlOutput => WL_OUTPUT,
        ObjectType::WlSubcompositor => WL_SUBCOMPOSITOR,
        ObjectType::WlSubsurface => WL_SUBSURFACE,
        ObjectType::WlDataDeviceManager => WL_DATA_DEVICE_MANAGER,
        ObjectType::WlDataDevice => WL_DATA_DEVICE,
        ObjectType::WlDataSource => WL_DATA_SOURCE,
        ObjectType::WlDataOffer => WL_DATA_OFFER,
        ObjectType::ZwpPrimarySelectionDeviceManager => ZWP_PRIMARY_SELECTION_DEVICE_MANAGER,
        ObjectType::ZwpPrimarySelectionDevice => ZWP_PRIMARY_SELECTION_DEVICE,
        ObjectType::ZwpPrimarySelectionSource => ZWP_PRIMARY_SELECTION_SOURCE,
        ObjectType::ZwpPrimarySelectionOffer => ZWP_PRIMARY_SELECTION_OFFER,
        ObjectType::ZwlrDataControlManager => ZWLR_DATA_CONTROL_MANAGER,
        ObjectType::ZwlrDataControlDevice => ZWLR_DATA_CONTROL_DEVICE,
        ObjectType::ZwlrDataControlSource => ZWLR_DATA_CONTROL_SOURCE,
        ObjectType::ZwlrDataControlOffer => ZWLR_DATA_CONTROL_OFFER,
        ObjectType::WlFixes => WL_FIXES,
        ObjectType::XdgWmBase => XDG_WM_BASE,
        ObjectType::XdgSurface => XDG_SURFACE,
        ObjectType::XdgToplevel => XDG_TOPLEVEL,
        ObjectType::XdgPopup => XDG_POPUP,
        ObjectType::XdgPositioner => XDG_POSITIONER,
        ObjectType::XdgSystemBell => XDG_SYSTEM_BELL,
        ObjectType::WpViewporter => WP_VIEWPORTER,
        ObjectType::WpViewport => WP_VIEWPORT,
        ObjectType::WpCursorShapeManager => WP_CURSOR_SHAPE_MANAGER,
        ObjectType::WpCursorShapeDevice => WP_CURSOR_SHAPE_DEVICE,
        ObjectType::WpFractionalScaleManager => WP_FRACTIONAL_SCALE_MANAGER,
        ObjectType::WpFractionalScale => WP_FRACTIONAL_SCALE,
        ObjectType::WpPresentation => WP_PRESENTATION,
        ObjectType::WpPresentationFeedback => WP_PRESENTATION_FEEDBACK,
        ObjectType::ZwpLinuxDmabuf => ZWP_LINUX_DMABUF,
        ObjectType::ZwpLinuxBufferParams => ZWP_LINUX_BUFFER_PARAMS,
        ObjectType::ZxdgDecorationManager => ZXDG_DECORATION_MANAGER,
        ObjectType::ZxdgToplevelDecoration => ZXDG_TOPLEVEL_DECORATION,
        ObjectType::ZwlrLayerShell => ZWLR_LAYER_SHELL,
        ObjectType::ZwlrLayerSurface => ZWLR_LAYER_SURFACE,
    }
}

/// The request an opcode names, or `None` if the interface has no such opcode.
pub fn request_at(object_type: ObjectType, op_code: u16) -> Option<&'static Request> {
    requests(object_type).get(op_code as usize)
}
