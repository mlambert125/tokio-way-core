//! `wl_registry` protocol handler.
//!
//! Advertises available globals to clients and handles bind requests,
//! which create new protocol objects for specific interfaces (`wl_shm`,
//! `wl_compositor`, `xdg_wm_base`, etc.).

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::CompositorState;
use super::wire_utils::{ArgReader, ArgWriter, build_message};
use super::{
    GLOBALS, ObjectType, WL_OUTPUT_INTERFACE, wl_output, wl_seat, wl_shm, wp_cursor_shape_manager,
    wp_fractional_scale_manager, zwlr_data_control_manager, zwlr_layer_shell, zwp_linux_dmabuf,
    zwp_primary_selection_device_manager,
};
use tokio_way_backends::outputs::OutputId;

// Request opcodes
const BIND: u16 = 0;

// Event opcodes
pub const GLOBAL: u16 = 0;
pub const GLOBAL_REMOVE: u16 = 1;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        BIND => handle_bind(state, msg),
        _ => super::unknown_request(state, msg, "wl_registry"),
    }
}

/// The interface a registry name was advertised under, or `None` if nothing
/// was advertised under it at all.
///
/// Globals come from three places — the static table, dma-buf once the backend
/// has answered, and one per output — and a name means whatever the place it
/// came from says it means. Answering that in one function is what lets
/// [`handle_bind`] check a client's claim before acting on it, rather than
/// each of the three branches deciding for itself.
fn advertised_interface(
    global_name: u32,
    dmabuf_global: Option<u32>,
    mut output_globals: impl Iterator<Item = u32>,
) -> Option<&'static str> {
    if let Some(global) = usize::try_from(global_name)
        .ok()
        .and_then(|i| GLOBALS.get(i))
    {
        return Some(global.interface);
    }
    if dmabuf_global == Some(global_name) {
        return Some(zwp_linux_dmabuf::INTERFACE);
    }
    output_globals
        .any(|name| name == global_name)
        .then_some(WL_OUTPUT_INTERFACE)
}

/// The object a static global's interface name creates when bound.
///
/// `None` for an interface listed in [`GLOBALS`] with no handler behind it,
/// which is a compositor bug rather than a client one: advertising something
/// and then refusing to bind it is worse than not advertising it.
fn static_object_type(interface: &str) -> Option<ObjectType> {
    Some(match interface {
        "wl_shm" => ObjectType::WlShm,
        "wl_compositor" => ObjectType::WlCompositor,
        "wl_subcompositor" => ObjectType::WlSubcompositor,
        "wl_data_device_manager" => ObjectType::WlDataDeviceManager,
        zwp_primary_selection_device_manager::INTERFACE => {
            ObjectType::ZwpPrimarySelectionDeviceManager
        }
        zwlr_data_control_manager::INTERFACE => ObjectType::ZwlrDataControlManager,
        "xdg_wm_base" => ObjectType::XdgWmBase,
        "xdg_system_bell_v1" => ObjectType::XdgSystemBell,
        "wl_seat" => ObjectType::WlSeat,
        "wp_viewporter" => ObjectType::WpViewporter,
        "wp_presentation" => ObjectType::WpPresentation,
        wp_fractional_scale_manager::INTERFACE => ObjectType::WpFractionalScaleManager,
        wp_cursor_shape_manager::INTERFACE => ObjectType::WpCursorShapeManager,
        "wl_fixes" => ObjectType::WlFixes,
        "zxdg_decoration_manager_v1" => ObjectType::ZxdgDecorationManager,
        zwlr_layer_shell::INTERFACE => ObjectType::ZwlrLayerShell,
        _ => return None,
    })
}

fn handle_bind(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    // Pre-collect output global mappings to avoid borrow conflicts later.
    let output_globals: Vec<(u32, OutputId)> = state
        .output_global_names
        .iter()
        .map(|(&id, &name)| (name, id))
        .collect();
    let dmabuf_global = state.dmabuf_global_name;

    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };

    let mut args = ArgReader::new(&msg.message.args);
    // bind args: u32 name, str interface, u32 version, u32 new_id
    let (Some(global_name), Some(interface), Some(version), Some(new_id)) =
        (args.u32(), args.string(), args.u32(), args.new_id())
    else {
        client.send_error(msg.message.object_id, 0, "wl_registry.bind: malformed args");
        return;
    };

    debug!(
        "wl_registry.bind: name={} interface={} new_id={}",
        global_name, interface, new_id
    );

    let advertised = advertised_interface(
        global_name,
        dmabuf_global,
        output_globals.iter().map(|&(name, _)| name),
    );

    // The client names the interface as well as the name, and the two must
    // agree. Binding by name alone and discarding the string looks harmless —
    // the name is what decides the type either way — but it is what turns a
    // client that has got its globals out of step into one holding a working
    // object of a type it does not expect, whose every later request decodes
    // against the wrong interface. That fault surfaces far from its cause,
    // whereas the client's own claim about what it thinks it is binding is
    // free evidence sitting right here.
    //
    // Version zero is refused with it: a client cannot speak version zero of
    // anything, and clamping it would leave every version-gated event
    // suppressed for the life of the object.
    if !advertised.is_some_and(|advertised| advertised == interface && version > 0) {
        client.send_error(
            msg.message.object_id,
            super::ERROR_INVALID_OBJECT,
            &format!(
                "wl_registry.bind: global {global_name} is not \"{interface}\" version {version}"
            ),
        );
        return;
    }

    // Check static globals first.
    if (global_name as usize) < GLOBALS.len() {
        let global = &GLOBALS[global_name as usize];
        let bound_version = version.min(global.version);

        // One register for every static global; only the interface differs.
        let Some(object_type) = static_object_type(global.interface) else {
            tracing::warn!(
                "wl_registry.bind: no handler for interface '{}' yet",
                global.interface,
            );
            return;
        };

        if client
            .register_with_version(new_id, object_type, bound_version)
            .is_err()
        {
            return;
        }

        // Interfaces that push initial state to the client on bind.
        match object_type {
            ObjectType::WlShm => wl_shm::send_formats(client, new_id),
            ObjectType::WlSeat => wl_seat::send_seat_info(state, msg.client_id, new_id),
            ObjectType::WpPresentation => {
                super::wp_presentation::send_clock_id(state, msg.client_id, new_id);
            }
            _ => {}
        }
    } else if dmabuf_global == Some(global_name) {
        let bound_version = version.min(zwp_linux_dmabuf::VERSION);
        if client
            .register_with_version(new_id, ObjectType::ZwpLinuxDmabuf, bound_version)
            .is_err()
        {
            return;
        }
        // NLL: client borrow ends here. The format list lives in state, which
        // is why this cannot be sent through the borrow above.
        zwp_linux_dmabuf::send_formats(state, msg.client_id, new_id);
    } else if let Some(&(_, output_id)) =
        output_globals.iter().find(|(name, _)| *name == global_name)
    {
        // Dynamic output global — bind to the specific output.
        let bound_version = version.min(super::WL_OUTPUT_VERSION);
        if client
            .register_with_version(new_id, ObjectType::WlOutput, bound_version)
            .is_err()
        {
            return;
        }
        // NLL: client borrow ends here
        state
            .output_bindings
            .insert((msg.client_id, new_id), output_id);
        wl_output::send_output_info(state, msg.client_id, new_id, output_id);
    } else {
        client.send_error(
            msg.message.object_id,
            0,
            &format!("wl_registry.bind: unknown global name {global_name}"),
        );
    }
}

/// Send `wl_registry.global` events for all static globals and dynamic output globals.
pub fn advertise_globals(state: &mut CompositorState, client_id: u32, registry_id: u32) {
    // Collect output global names before borrowing client.
    let output_globals: Vec<u32> = state.output_global_names.values().copied().collect();
    let dmabuf_global = state.dmabuf_global_name;

    let Some(client) = state.clients.get(client_id) else {
        return;
    };

    // Static globals
    for (id, global) in (0u32..).zip(GLOBALS.iter()) {
        let args = ArgWriter::new()
            .u32(id)
            .string(global.interface)
            .u32(global.version)
            .build();
        if client
            .send(build_message(registry_id, GLOBAL, args))
            .is_err()
        {
            return;
        }
    }

    // Advertised only once the backend has said it can import one, which may
    // be after this client connected — hence the broadcast path as well.
    if let Some(global_name) = dmabuf_global {
        let args = ArgWriter::new()
            .u32(global_name)
            .string(zwp_linux_dmabuf::INTERFACE)
            .u32(zwp_linux_dmabuf::VERSION)
            .build();
        if client
            .send(build_message(registry_id, GLOBAL, args))
            .is_err()
        {
            return;
        }
    }

    // Dynamic output globals (one per physical output)
    for global_name in output_globals {
        let args = ArgWriter::new()
            .u32(global_name)
            .string(WL_OUTPUT_INTERFACE)
            .u32(super::WL_OUTPUT_VERSION)
            .build();
        if client
            .send(build_message(registry_id, GLOBAL, args))
            .is_err()
        {
            return;
        }
    }
}

/// Broadcast a global that has appeared since the clients did.
///
/// Most globals exist before any client connects and are listed in
/// `advertise_globals`. Some cannot: an output arrives when the display is
/// plugged in, and dma-buf support is only known once the backend has a GL
/// context to ask. Those are announced to whoever is already connected here,
/// and by `advertise_globals` to whoever connects later.
pub fn broadcast_global(
    state: &mut CompositorState,
    global_name: u32,
    interface: &str,
    version: u32,
) {
    for (_, client) in state.clients.iter() {
        for (obj_id, obj_type) in &client.objects {
            if *obj_type == ObjectType::WlRegistry {
                let args = ArgWriter::new()
                    .u32(global_name)
                    .string(interface)
                    .u32(version)
                    .build();
                let _ = client.send(build_message(*obj_id, GLOBAL, args));
            }
        }
    }
}

/// Withdraw a global that has gone away.
///
/// The counterpart to [`broadcast_global`], and the half that was missing: a
/// global announced and never withdrawn leaves every client holding a name it
/// may still try to bind, and an object for a thing that no longer exists. An
/// unplugged display is the case this exists for.
///
/// A client is expected to destroy its objects for the global on hearing this,
/// and the compositor must send it nothing further about them — which is why
/// the bookkeeping tying clients to the departed output is dropped alongside,
/// in [`CompositorState::remove_output`].
pub fn broadcast_global_remove(state: &mut CompositorState, global_name: u32) {
    for (_, client) in state.clients.iter() {
        for (obj_id, obj_type) in &client.objects {
            if *obj_type == ObjectType::WlRegistry {
                let args = ArgWriter::new().u32(global_name).build();
                let _ = client.send(build_message(*obj_id, GLOBAL_REMOVE, args));
            }
        }
    }
}
