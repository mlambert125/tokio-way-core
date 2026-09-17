//! `wl_fixes` protocol handler.
//!
//! One global for problems in the core protocol that could not be fixed where
//! they arose. It carries a single repair so far: a way to destroy a
//! `wl_registry`.
//!
//! `wl_registry` has no `destroy` request of its own, which is an oversight
//! rather than a design — a client that binds a registry can never give it
//! back, so the compositor keeps sending it `global` and `global_remove` for
//! the life of the connection and the id is never freed. `destroy_registry`
//! is the way out, and it has to live on a separate interface because adding
//! a request to `wl_registry` would have changed a version every client
//! already depends on.

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::CompositorState;
use super::ObjectType;
use super::wire_utils::ArgReader;

/// The interface name, for the registry.
pub const INTERFACE: &str = "wl_fixes";
/// The version advertised.
pub const VERSION: u32 = 1;

// Request opcodes
const DESTROY: u16 = 0;
const DESTROY_REGISTRY: u16 = 1;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        DESTROY => {
            if let Some(client) = state.clients.get(msg.client_id) {
                client.unregister(msg.message.object_id);
            } else {
                tracing::warn!("Received message from unknown client {}", msg.client_id);
            }
        }
        DESTROY_REGISTRY => handle_destroy_registry(state, msg),
        _ => super::reject_unknown_request(state, msg, "wl_fixes"),
    }
}

fn handle_destroy_registry(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(registry_id) = args.u32() else {
        super::reject_malformed_request(state, msg, "wl_fixes");
        return;
    };

    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };

    // The id must name a registry of this client's. Anything else is the
    // client and the compositor disagreeing about what that object is, which
    // is the same disagreement any wrong object id represents.
    if client.objects.get(&registry_id) != Some(&ObjectType::WlRegistry) {
        client.send_error(
            msg.message.object_id,
            super::ERROR_INVALID_OBJECT,
            "wl_fixes.destroy_registry: not a wl_registry of this client",
        );
        return;
    }

    debug!("wl_fixes.destroy_registry: registry_id={registry_id}");
    // Unregistering is the whole of it: `broadcast_global` and
    // `broadcast_global_remove` both find their targets by walking the client's
    // objects for registries, so a registry that is gone from that map stops
    // being sent to by construction.
    client.unregister(registry_id);
}
