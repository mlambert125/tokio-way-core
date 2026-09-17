//! Lookup tables of every request the compositor advertises, in opcode order, with
//! the arguments it carries.

use super::ObjectType;

/// Argument Type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgType {
    /// A signed 32-bit integer
    Int,
    /// An unsigned 32-bit integer
    Uint,
    /// 24.8 signed fixed point
    Fixed,
    /// A wayland string - A length, the bytes, a null, and padding to four
    String,
    /// The id of an existing object, or zero for null
    Object,
    /// An id the client is allocating
    NewId,
    /// A length and that many bytes, padded to four
    Array,
    /// A descriptor, sent out of band and occupying no wire bytes
    Fd,
}

/// A wayland request
#[derive(Debug, Clone, Copy)]
pub struct Request {
    /// Request's name in the protocol
    pub name: &'static str,
    /// Arguments' types, in the order they appear
    pub args: &'static [ArgType],
}

use ArgType::{Fd, Fixed, Int, NewId, Object, String as Str, Uint};

/// Shorthand for one entry, so the tables below read as the protocol does
const fn request(name: &'static str, args: &'static [ArgType]) -> Request {
    Request { name, args }
}

const WL_DISPLAY: &[Request] = &[request("sync", &[NewId]), request("get_registry", &[NewId])];
const WL_REGISTRY: &[Request] = &[request("bind", &[Uint, Str, Uint, NewId])];
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
const WL_FIXES: &[Request] = &[
    request("destroy", &[]),
    request("destroy_registry", &[Object]),
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
    request("get_tablet_tool_v2", &[NewId, Object]),
];

const WP_CURSOR_SHAPE_DEVICE: &[Request] =
    &[request("destroy", &[]), request("set_shape", &[Uint, Uint])];

const WP_FRACTIONAL_SCALE_MANAGER: &[Request] = &[
    request("destroy", &[]),
    request("get_fractional_scale", &[NewId, Object]),
];

const WP_FRACTIONAL_SCALE: &[Request] = &[request("destroy", &[])];

const WP_PRESENTATION: &[Request] = &[
    request("destroy", &[]),
    request("feedback", &[Object, NewId]),
];

const WP_PRESENTATION_FEEDBACK: &[Request] = &[];

const ZWP_LINUX_DMABUF: &[Request] = &[request("destroy", &[]), request("create_params", &[NewId])];

const ZWP_LINUX_BUFFER_PARAMS: &[Request] = &[
    request("destroy", &[]),
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

/// Lookup by `ObjectType`
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

/// Lookup a specific operation by `ObjectType` and op code
pub fn request_at(object_type: ObjectType, op_code: u16) -> Option<&'static Request> {
    requests(object_type).get(op_code as usize)
}
