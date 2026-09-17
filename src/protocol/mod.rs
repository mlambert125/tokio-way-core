//! Protocol module root.

use super::state::{ClientObjectId, CompositorState, DataInterface};
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio_way_sock::WaylandRequestWithClientInfo;
pub use wire_utils::{ArgReader, ArgWriter, build_message};

pub mod signature;
pub mod wire_utils;
pub mod wl_buffer;
pub mod wl_callback;
pub mod wl_compositor;
pub mod wl_data_device;
pub mod wl_data_device_manager;
pub mod wl_data_offer;
pub mod wl_data_source;
pub mod wl_display;
pub mod wl_fixes;
pub mod wl_keyboard;
pub mod wl_output;
pub mod wl_pointer;
pub mod wl_region;
pub mod wl_registry;
pub mod wl_seat;
pub mod wl_shm;
pub mod wl_shm_pool;
pub mod wl_subcompositor;
pub mod wl_subsurface;
pub mod wl_surface;
pub mod wl_touch;
pub mod wp_cursor_shape_device;
pub mod wp_cursor_shape_manager;
pub mod wp_fractional_scale;
pub mod wp_fractional_scale_manager;
pub mod wp_presentation;
pub mod wp_presentation_feedback;
pub mod wp_viewport;
pub mod wp_viewporter;
pub mod xdg_popup;
pub mod xdg_positioner;
pub mod xdg_surface;
pub mod xdg_system_bell;
pub mod xdg_toplevel;
pub mod xdg_wm_base;
pub mod zwlr_data_control_device;
pub mod zwlr_data_control_manager;
pub mod zwlr_data_control_offer;
pub mod zwlr_data_control_source;
pub mod zwlr_layer_shell;
pub mod zwlr_layer_surface;
pub mod zwp_linux_buffer_params;
pub mod zwp_linux_dmabuf;
pub mod zwp_primary_selection_device;
pub mod zwp_primary_selection_device_manager;
pub mod zwp_primary_selection_offer;
pub mod zwp_primary_selection_source;
pub mod zxdg_decoration_manager;
pub mod zxdg_toplevel_decoration;

/// Atomic serial number to track event and operation ordering
static NEXT_SERIAL: AtomicU32 = AtomicU32::new(1);

/// Helper method for getting the next atomic number
pub fn next_serial() -> u32 {
    NEXT_SERIAL.fetch_add(1, Ordering::Relaxed)
}

/// `wl_display.error` - The object named does not exist, or the client is not allowed to name it.
pub const ERROR_INVALID_OBJECT: u32 = 0;
/// `wl_display.error` - The object exists, but the request named is not one of its own.
pub const ERROR_INVALID_METHOD: u32 = 1;

/// Reject a request whose opcode the interface does not have.
pub fn reject_unknown_request(
    state: &mut CompositorState,
    msg: &WaylandRequestWithClientInfo,
    interface: &str,
) {
    let op_code = msg.message.op_code;
    let object_id = msg.message.object_id;
    tracing::warn!(
        "client {}: {interface} has no request {op_code} (object {object_id})",
        msg.client_id,
    );
    if let Some(client) = state.clients.get(msg.client_id) {
        client.send_error(
            object_id,
            ERROR_INVALID_METHOD,
            &format!("{interface} has no request {op_code}"),
        );
    }
}

/// Reject a request whose arguments do not decode.
pub fn reject_malformed_request(
    state: &mut CompositorState,
    msg: &WaylandRequestWithClientInfo,
    interface: &str,
) {
    let op_code = msg.message.op_code;
    let object_id = msg.message.object_id;

    let named = state
        .clients
        .get(msg.client_id)
        .and_then(|client| client.objects.get(&object_id).copied())
        .and_then(|obj_type| signature::request_at(obj_type, op_code))
        .map_or_else(
            || format!("{interface} request {op_code}"),
            |request| format!("{interface}.{}", request.name),
        );
    tracing::warn!(
        "client {}: malformed arguments for {named} (object {object_id})",
        msg.client_id,
    );
    if let Some(client) = state.clients.get(msg.client_id) {
        client.send_error(
            object_id,
            ERROR_INVALID_METHOD,
            &format!("malformed arguments for {named}"),
        );
    }
}

/// Ask whichever interface owns a data source to hand over its content.
pub fn send_source_content(
    state: &mut CompositorState,
    source: ClientObjectId,
    mime_type: &str,
    fd: OwnedFd,
) {
    match state.data_sources.get(&source).map(|s| s.interface) {
        Some(DataInterface::WlData) => wl_data_source::send_send(state, source, mime_type, fd),
        Some(DataInterface::PrimarySelection) => {
            zwp_primary_selection_source::send_send(state, source, mime_type, fd);
        }
        Some(DataInterface::DataControl) => {
            zwlr_data_control_source::send_send(state, source, mime_type, fd);
        }
        None => drop(fd),
    }
}

/// Tell whichever interface owns a source that it has been cancelled.
pub fn cancel_source(state: &mut CompositorState, source: ClientObjectId) {
    if let Some(source) = state.data_sources.get_mut(&source) {
        source.cancelled = true;
    }
    match state.data_sources.get(&source).map(|s| s.interface) {
        Some(DataInterface::WlData) => wl_data_source::send_cancelled(state, source),
        Some(DataInterface::PrimarySelection) => {
            zwp_primary_selection_source::send_cancelled(state, source);
        }
        Some(DataInterface::DataControl) => zwlr_data_control_source::send_cancelled(state, source),
        None => {}
    }
}

/// The type of a Wayland protocol object.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectType {
    WlDisplay,
    WlRegistry,
    WlFixes,
    WlCallback,
    WlShm,
    WlShmPool,
    WlBuffer,
    WlCompositor,
    WlSurface,
    WlRegion,
    WlSeat,
    WlPointer,
    WlKeyboard,
    WlTouch,
    WlOutput,
    WlSubcompositor,
    WlSubsurface,
    WlDataDeviceManager,
    WlDataDevice,
    WlDataSource,
    WlDataOffer,
    XdgWmBase,
    XdgSurface,
    XdgSystemBell,
    XdgToplevel,
    XdgPopup,
    XdgPositioner,
    WpPresentation,
    WpPresentationFeedback,
    WpViewporter,
    WpViewport,
    WpFractionalScaleManager,
    WpFractionalScale,
    WpCursorShapeManager,
    WpCursorShapeDevice,
    ZxdgDecorationManager,
    ZxdgToplevelDecoration,
    ZwpLinuxDmabuf,
    ZwpLinuxBufferParams,
    ZwpPrimarySelectionDeviceManager,
    ZwpPrimarySelectionDevice,
    ZwpPrimarySelectionSource,
    ZwpPrimarySelectionOffer,
    ZwlrDataControlManager,
    ZwlrDataControlDevice,
    ZwlrDataControlSource,
    ZwlrDataControlOffer,
    ZwlrLayerShell,
    ZwlrLayerSurface,
}

/// A global interface that clients can bind via `wl_registry`.
pub struct Global {
    pub interface: &'static str,
    pub version: u32,
}

/// The `wl_output` version we support. These are globals, but there's one per monitor.
pub const WL_OUTPUT_VERSION: u32 = 4;

/// The interface name those dynamic globals are advertised under.
pub const WL_OUTPUT_INTERFACE: &str = "wl_output";

/// Static globals provided by the compositor.
pub static GLOBALS: &[Global] = &[
    Global {
        interface: "wl_compositor",
        version: 5,
    },
    Global {
        interface: "wl_subcompositor",
        version: 1,
    },
    Global {
        interface: "wl_data_device_manager",
        version: 3,
    },
    Global {
        interface: zwp_primary_selection_device_manager::INTERFACE,
        version: zwp_primary_selection_device_manager::VERSION,
    },
    Global {
        interface: zwlr_data_control_manager::INTERFACE,
        version: zwlr_data_control_manager::VERSION,
    },
    Global {
        interface: "wl_shm",
        version: 1,
    },
    Global {
        interface: "wl_seat",
        version: 8,
    },
    Global {
        interface: "xdg_wm_base",
        version: 5,
    },
    Global {
        interface: "xdg_system_bell_v1",
        version: 1,
    },
    Global {
        interface: "wp_viewporter",
        version: 1,
    },
    Global {
        interface: "wp_presentation",
        version: 1,
    },
    Global {
        interface: wp_fractional_scale_manager::INTERFACE,
        version: wp_fractional_scale_manager::VERSION,
    },
    Global {
        interface: wp_cursor_shape_manager::INTERFACE,
        version: wp_cursor_shape_manager::VERSION,
    },
    Global {
        interface: wl_fixes::INTERFACE,
        version: wl_fixes::VERSION,
    },
    Global {
        interface: "zxdg_decoration_manager_v1",
        version: 1,
    },
    Global {
        interface: zwlr_layer_shell::INTERFACE,
        version: zwlr_layer_shell::VERSION,
    },
];

/// Number of file descriptors a request carries as ancillary data.
fn request_fd_count(obj_type: ObjectType, op_code: u16) -> usize {
    signature::request_at(obj_type, op_code).map_or(0, |request| {
        request
            .args
            .iter()
            .filter(|arg| **arg == signature::ArgType::Fd)
            .count()
    })
}

/// Dispatch an individual message coming from the socket to the appropriate handler
#[allow(clippy::too_many_lines)]
pub fn handle_message(state: &mut CompositorState, message: &WaylandRequestWithClientInfo) {
    let object_id = message.message.object_id;
    let client_id = message.client_id;
    let Some(client) = state.clients.get(client_id) else {
        tracing::warn!("Received message from unknown client {}", message.client_id);
        return;
    };

    let obj_type = client.objects.get(&object_id).copied();

    let mut request_fds: Vec<OwnedFd> = Vec::new();
    if let Some(obj_type) = obj_type {
        let count = request_fd_count(obj_type, message.message.op_code);
        if count > 0 {
            if client.fd_queue.len() < count {
                tracing::warn!(
                    "client {}: request for object {} is missing its file descriptor",
                    client_id,
                    object_id,
                );
                // WL_DISPLAY_ERROR_INVALID_METHOD = 1
                client.send_error(object_id, 1, "request is missing its file descriptor");
                return;
            }
            request_fds = client.fd_queue.drain(..count).collect();
        }
    }

    match obj_type {
        Some(ObjectType::WlDisplay) => {
            wl_display::handle(state, message);
        }
        Some(ObjectType::WlRegistry) => {
            wl_registry::handle(state, message);
        }
        Some(ObjectType::WlFixes) => {
            wl_fixes::handle(state, message);
        }
        Some(ObjectType::WlCallback) => {
            wl_callback::handle(state, message);
        }
        Some(ObjectType::WlShm) => {
            wl_shm::handle(state, message, request_fds);
        }
        Some(ObjectType::WlShmPool) => {
            wl_shm_pool::handle(state, message);
        }
        Some(ObjectType::WlCompositor) => {
            wl_compositor::handle(state, message);
        }
        Some(ObjectType::WlSurface) => {
            wl_surface::handle(state, message);
        }
        Some(ObjectType::WlRegion) => {
            wl_region::handle(state, message);
        }
        Some(ObjectType::WlSubcompositor) => {
            wl_subcompositor::handle(state, message);
        }
        Some(ObjectType::WlSubsurface) => {
            wl_subsurface::handle(state, message);
        }
        Some(ObjectType::WlDataDeviceManager) => {
            wl_data_device_manager::handle(state, message);
        }
        Some(ObjectType::WlDataDevice) => {
            wl_data_device::handle(state, message);
        }
        Some(ObjectType::WlDataSource) => {
            wl_data_source::handle(state, message);
        }
        Some(ObjectType::WlDataOffer) => {
            wl_data_offer::handle(state, message, request_fds);
        }
        Some(ObjectType::ZwpPrimarySelectionDeviceManager) => {
            zwp_primary_selection_device_manager::handle(state, message);
        }
        Some(ObjectType::ZwpPrimarySelectionDevice) => {
            zwp_primary_selection_device::handle(state, message);
        }
        Some(ObjectType::ZwpPrimarySelectionSource) => {
            zwp_primary_selection_source::handle(state, message);
        }
        Some(ObjectType::ZwpPrimarySelectionOffer) => {
            zwp_primary_selection_offer::handle(state, message, request_fds);
        }
        Some(ObjectType::ZwlrDataControlManager) => {
            zwlr_data_control_manager::handle(state, message);
        }
        Some(ObjectType::ZwlrDataControlDevice) => {
            zwlr_data_control_device::handle(state, message);
        }
        Some(ObjectType::ZwlrDataControlSource) => {
            zwlr_data_control_source::handle(state, message);
        }
        Some(ObjectType::ZwlrDataControlOffer) => {
            zwlr_data_control_offer::handle(state, message, request_fds);
        }
        Some(ObjectType::WlSeat) => {
            wl_seat::handle(state, message);
        }
        Some(ObjectType::WlPointer) => {
            wl_pointer::handle(state, message);
        }
        Some(ObjectType::WlKeyboard) => {
            wl_keyboard::handle(state, message);
        }
        Some(ObjectType::WlTouch) => {
            wl_touch::handle(state, message);
        }
        Some(ObjectType::WlOutput) => {
            wl_output::handle(state, message);
        }
        Some(ObjectType::XdgWmBase) => {
            xdg_wm_base::handle(state, message);
        }
        Some(ObjectType::XdgSurface) => {
            xdg_surface::handle(state, message);
        }
        Some(ObjectType::XdgSystemBell) => {
            xdg_system_bell::handle(state, message);
        }
        Some(ObjectType::XdgToplevel) => {
            xdg_toplevel::handle(state, message);
        }
        Some(ObjectType::XdgPopup) => {
            xdg_popup::handle(state, message);
        }
        Some(ObjectType::XdgPositioner) => {
            xdg_positioner::handle(state, message);
        }
        Some(ObjectType::WlBuffer) => {
            wl_buffer::handle(state, message);
        }
        Some(ObjectType::WpPresentation) => {
            wp_presentation::handle(state, message);
        }
        Some(ObjectType::WpPresentationFeedback) => {
            wp_presentation_feedback::handle(state, message);
        }
        Some(ObjectType::WpCursorShapeManager) => {
            wp_cursor_shape_manager::handle(state, message);
        }
        Some(ObjectType::WpCursorShapeDevice) => {
            wp_cursor_shape_device::handle(state, message);
        }
        Some(ObjectType::WpFractionalScaleManager) => {
            wp_fractional_scale_manager::handle(state, message);
        }
        Some(ObjectType::WpFractionalScale) => {
            wp_fractional_scale::handle(state, message);
        }
        Some(ObjectType::WpViewporter) => {
            wp_viewporter::handle(state, message);
        }
        Some(ObjectType::WpViewport) => {
            wp_viewport::handle(state, message);
        }
        Some(ObjectType::ZwpLinuxDmabuf) => {
            zwp_linux_dmabuf::handle(state, message);
        }
        Some(ObjectType::ZwpLinuxBufferParams) => {
            zwp_linux_buffer_params::handle(state, message, request_fds);
        }
        Some(ObjectType::ZxdgDecorationManager) => {
            zxdg_decoration_manager::handle(state, message);
        }
        Some(ObjectType::ZxdgToplevelDecoration) => {
            zxdg_toplevel_decoration::handle(state, message);
        }
        Some(ObjectType::ZwlrLayerShell) => {
            zwlr_layer_shell::handle(state, message);
        }
        Some(ObjectType::ZwlrLayerSurface) => {
            zwlr_layer_surface::handle(state, message);
        }
        None => {
            tracing::warn!(
                "client {}: unknown object_id={}, op_code={}",
                message.client_id,
                object_id,
                message.message.op_code,
            );
            client.send_error(
                object_id,
                ERROR_INVALID_OBJECT,
                &format!("invalid object {object_id}"),
            );
        }
    }
}
