//! `zwp_linux_dmabuf_v1` protocol handler.
//!
//! How a client hands the compositor a buffer it has already rendered on the
//! GPU. The client describes the descriptors through a
//! [`zwp_linux_buffer_params_v1`](super::zwp_linux_buffer_params) and gets back
//! a `wl_buffer` it can attach like any other; nothing is copied, and the
//! backend samples the client's own memory.
//!
//! Version 3 is what is advertised. On bind the compositor announces what it
//! can import: `modifier` events for a version 3 client, which carry the layout
//! alongside the format, and `format` events for an older one, which cannot say
//! anything about layout. Version 4 forbids both and replaces them with
//! `zwp_linux_dmabuf_feedback_v1` — a format table over a descriptor, a main
//! device, and per-surface tranches — which is a good deal more machinery for
//! clients that all fall back to version 3 cleanly.
//!
//! The global is advertised only once the backend has confirmed it can actually
//! import — see [`tokio_way_backends::dmabuf_import::DmabufImportProbeResult`]. Offering dma-buf a compositor
//! cannot draw is worse than offering none: the client allocates against the
//! list it is given and has nothing left to fall back to.

use tracing::debug;

use tokio_way_backends::dma::{DRM_FORMAT_MOD_INVALID, DmabufFormat};
use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::{BufferParams, CompositorState};
use super::ObjectType;
use super::wire_utils::{ArgReader, ArgWriter, build_message};

/// The interface name, as it goes out in `wl_registry.global`.
pub const INTERFACE: &str = "zwp_linux_dmabuf_v1";
/// The version advertised. See the module header for why not 4.
pub const VERSION: u32 = 3;
/// First version that takes `modifier` events instead of `format` ones.
const MODIFIER_VERSION: u32 = 3;

// Request opcodes
const DESTROY: u16 = 0;
const CREATE_PARAMS: u16 = 1;

// Event opcodes
const FORMAT: u16 = 0;
const MODIFIER: u16 = 1;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        DESTROY => handle_destroy(state, msg),
        CREATE_PARAMS => handle_create_params(state, msg),
        _ => super::reject_unknown_request(state, msg, "zwp_linux_dmabuf_v1"),
    }
}

/// Destroying the factory leaves the objects it made alone: a `wl_buffer`
/// outlives the params it came from, and those outlive this.
fn handle_destroy(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };
    client.unregister(msg.message.object_id);
}

/// Start describing a buffer.
fn handle_create_params(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    // Each params object can pin a descriptor per plane and is destroyed only
    // when the client says so, so an unbounded number of them is an unbounded
    // number of open descriptors. An shm pool costs one fd however large it is;
    // this costs one per plane, per object.
    const MAX_LIVE_PARAMS: usize = 64;

    let live = state
        .dmabuf_params
        .keys()
        .filter(|(cid, _)| *cid == msg.client_id)
        .count();

    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };

    let mut args = ArgReader::new(&msg.message.args);
    let Some(params_id) = args.new_id() else {
        client.send_error(
            msg.message.object_id,
            0,
            "zwp_linux_dmabuf_v1.create_params: malformed args",
        );
        return;
    };

    if live >= MAX_LIVE_PARAMS {
        client.send_error(
            msg.message.object_id,
            0,
            "too many zwp_linux_buffer_params_v1 objects held open",
        );
        client.cancel_token.cancel();
        return;
    }

    if client
        .register_client_object(params_id, ObjectType::ZwpLinuxBufferParams)
        .is_err()
    {
        return;
    }
    debug!("zwp_linux_dmabuf_v1.create_params: id={params_id}");
    state
        .dmabuf_params
        .insert((msg.client_id, params_id), BufferParams::default());
}

/// Tell a client what can be imported, at the moment it binds.
///
/// Takes `state` rather than the client, because the format list lives in state
/// and the borrow checker will not have both — the same shape
/// `wl_seat::send_seat_info` uses.
pub fn send_formats(state: &mut CompositorState, client_id: u32, dmabuf_id: u32) {
    let formats = state.dmabuf_formats.clone();
    let Some(client) = state.clients.get(client_id) else {
        return;
    };
    let version = client.version(dmabuf_id);

    for format in &formats {
        if version < MODIFIER_VERSION {
            // All an old client can be told is that the format works; which
            // layouts do is not expressible before version 3.
            let args = ArgWriter::new().u32(format.fourcc).build();
            if client.send(build_message(dmabuf_id, FORMAT, args)).is_err() {
                return;
            }
            continue;
        }
        for modifier in advertised_modifiers(format) {
            let args = ArgWriter::new()
                .u32(format.fourcc)
                // Hi first: a modifier is 64 bits and an argument is 32.
                .u32(u32::try_from(modifier >> 32).unwrap_or(0))
                .u32(u32::try_from(modifier & 0xffff_ffff).unwrap_or(0))
                .build();
            if client
                .send(build_message(dmabuf_id, MODIFIER, args))
                .is_err()
            {
                return;
            }
        }
    }
}

/// The modifiers to advertise for one format.
///
/// `DRM_FORMAT_MOD_INVALID` is always among them: it is how a client says "no
/// explicit layout, work it out from the descriptor", which the importer
/// supports and which is the only thing a driver that named no modifiers can
/// take.
fn advertised_modifiers(format: &DmabufFormat) -> impl Iterator<Item = u64> {
    format
        .modifiers
        .iter()
        .copied()
        .chain(std::iter::once(DRM_FORMAT_MOD_INVALID))
}
