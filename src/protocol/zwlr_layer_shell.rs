//! `zwlr_layer_shell_v1` protocol handler.
//!
//! The shell a panel, bar, wallpaper or lock screen binds instead of
//! `xdg_wm_base`. What it adds over a toplevel is a *band*: a wallpaper has to
//! be under every window and a lock screen over every one, and `xdg_shell` has
//! no way to say either. See [`crate::layer`] for where the
//! surfaces actually land.

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::{CompositorState, Layer, LayerPending, LayerSurfaceState};
use super::ObjectType;
use super::wire_utils::ArgReader;

pub const INTERFACE: &str = "zwlr_layer_shell_v1";
/// Version 4. Version 5 adds `set_exclusive_edge`, which is not implemented,
/// and advertising a version whose requests are missing is worse than
/// advertising a lower one — a client would use it and be quietly ignored.
pub const VERSION: u32 = 4;

// Request opcodes
const GET_LAYER_SURFACE: u16 = 0;
const DESTROY: u16 = 1;

/// `zwlr_layer_shell_v1.error.role`: the surface already has one.
const ERROR_ROLE: u32 = 0;
/// `zwlr_layer_shell_v1.error.invalid_layer`.
const ERROR_INVALID_LAYER: u32 = 1;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        GET_LAYER_SURFACE => handle_get_layer_surface(state, msg),
        DESTROY => {
            if let Some(client) = state.clients.get(msg.client_id) {
                client.unregister(msg.message.object_id);
            }
        }
        _ => super::unknown_request(state, msg, INTERFACE),
    }
}

fn handle_get_layer_surface(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    // get_layer_surface(new_id, surface, output, layer, namespace)
    let (Some(layer_id), Some(surface_id), Some(output_id), Some(layer), Some(namespace)) = (
        args.new_id(),
        args.u32(),
        args.u32(),
        args.u32(),
        args.string(),
    ) else {
        super::malformed_request(state, msg, INTERFACE);
        return;
    };
    let client_id = msg.client_id;
    let surface = (client_id, surface_id);
    debug!(
        "zwlr_layer_shell_v1.get_layer_surface: id={layer_id} surface={surface_id} \
         layer={layer} namespace={namespace:?}"
    );

    let Some(layer) = Layer::from_repr(layer) else {
        if let Some(client) = state.clients.get(client_id) {
            client.send_error(
                msg.message.object_id,
                ERROR_INVALID_LAYER,
                &format!("zwlr_layer_shell_v1.get_layer_surface: {layer} is not a layer"),
            );
        }
        return;
    };

    // A surface has one role for its whole life. Handing a second to a surface
    // that is already a cursor, a drag icon, a subsurface or a window would
    // leave two subsystems each believing they place it.
    if !state.surfaces.contains_key(&surface) || state.has_role(surface) {
        if let Some(client) = state.clients.get(client_id) {
            client.send_error(
                msg.message.object_id,
                ERROR_ROLE,
                "zwlr_layer_shell_v1.get_layer_surface: the surface already has a role",
            );
        }
        return;
    }

    // A null output lets the compositor choose, which the protocol allows
    // outright. Resolved now rather than left as "somewhere": a surface with no
    // output has no size to be configured against.
    let output = if output_id == 0 {
        state.output_for_new_window()
    } else {
        state.output_bindings.get(&(client_id, output_id)).copied()
    };

    let Some(client) = state.clients.get(client_id) else {
        return;
    };
    let version = client.version(msg.message.object_id);
    if client
        .register_with_version(layer_id, ObjectType::ZwlrLayerSurface, version)
        .is_err()
    {
        return;
    }

    state.layer_surfaces.insert(
        (client_id, layer_id),
        LayerSurfaceState {
            client_id,
            wl_surface_id: surface_id,
            output,
            pending: LayerPending {
                layer,
                ..LayerPending::default()
            },
            current: LayerPending {
                layer,
                ..LayerPending::default()
            },
            namespace,
            configured: false,
            pending_configures: std::collections::VecDeque::new(),
            highest_configure: 0,
            configured_size: None,
        },
    );
    state.surface_layer.insert(surface, layer_id);
    state.dirty = true;
}
