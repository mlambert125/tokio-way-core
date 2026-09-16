//! Per-client connection state.
//!
//! Each connected client gets a `ClientState` tracking its object map (id ->
//! `ObjectType)` and a channel sender for pushing events back to the client.
//! The Clients struct manages the collection of all active client states.

use crate::protocol::wire_utils::{ArgWriter, build_message};
use crate::protocol::{ObjectType, wl_display};
use std::collections::{HashMap, VecDeque};
use std::os::fd::OwnedFd;
use tokio::sync::mpsc::{Sender, error::TrySendError};
use tokio_util::sync::CancellationToken;
use tokio_way_sock::WaylandEvent;

/// The most file descriptors a client may have queued but unclaimed before we
/// give up on it.
///
/// Requests that take no descriptor never drain the queue, so a client that
/// attaches them anyway grows it without bound. Descriptors are a process-wide
/// resource, so that is not merely the offending client's problem: exhausting
/// `RLIMIT_NOFILE` would break every other client, along with `mmap` and
/// `memfd_create`. libwayland bounds the same queue the same way.
///
/// The cap lives here rather than in the socket task because the queue does.
/// The socket hands over what each read delivered and keeps nothing, so it has
/// no idea how much of it has since been claimed.
///
/// It is set well above any legitimate burst: only two requests carry a
/// descriptor, and the compositor drains its whole channel on each pass of its
/// loop, so a well-behaved client never accumulates more than a handful.
pub const MAX_PENDING_FDS: usize = 256;

/// The first object id the compositor may hand out.
///
/// Wayland splits the id space: a client allocates from below this line and the
/// compositor from above it, so neither has to ask the other what is free. The
/// halves are why `wl_display.delete_id` exists only for the client's own ids:
/// a client needs telling which of *its* ids it may use again, and has no
/// business reusing one of ours. [`ClientState::register`] holds up the
/// client's side of that bargain, and [`ClientState::allocate_id`] simply never
/// reuses an id at all.
pub const SERVER_ID_BASE: u32 = 0xff00_0000;

/// Whether an object id belongs to the compositor's half of the id space.
pub fn is_server_id(id: u32) -> bool {
    id >= SERVER_ID_BASE
}

/// State specific to an individual wayland client
pub struct ClientState {
    /// Maps object id -> object type/state for every object this client has created.
    pub objects: HashMap<u32, ObjectType>,
    /// Maps object id -> bound interface version for version-gated events.
    pub object_versions: HashMap<u32, u32>,
    /// Sender for writing messages back to this client's socket.
    pub sender: Sender<WaylandEvent>,
    /// Descriptors this client has sent that no request has claimed yet, in
    /// arrival order.
    ///
    /// Fed by [`Self::queue_fds`] as each read arrives and drained at dispatch,
    /// where the request's interface says how many it takes. It outlives any
    /// one read on purpose: a `sendmsg` carrying two descriptors can straddle
    /// a read boundary, leaving one here until the request that claims it
    /// arrives.
    pub fd_queue: VecDeque<OwnedFd>,
    /// Cancels this client's socket tasks. Triggered when the client stops
    /// draining its socket and its outgoing queue overflows.
    pub cancel_token: CancellationToken,
    /// Next id to hand out from the compositor's half of the id space.
    next_server_id: u32,
}

impl ClientState {
    /// Create a new client state with a provided sender for talking back to
    /// the socket and a cancellation token for killing the underlying socket
    pub fn new(sender: Sender<WaylandEvent>, cancel_token: CancellationToken) -> Self {
        let mut objects = HashMap::new();
        objects.insert(wl_display::OBJECT_ID, ObjectType::WlDisplay);
        Self {
            objects,
            object_versions: HashMap::new(),
            sender,
            fd_queue: VecDeque::new(),
            cancel_token,
            next_server_id: SERVER_ID_BASE,
        }
    }

    /// Register a newly created object.
    ///
    /// Returns `Err` if the client is reusing an id that is still live. Clients
    /// must not reuse an object id until the compositor has acknowledged its
    /// destruction with `wl_display.delete_id`, so this is a protocol error.
    /// Silently replacing the old object — which is what a bare `insert` did —
    /// strands whatever compositor state it owned: an `mmap`ed shm pool, a
    /// surface in the stack, a pointer or keyboard binding. Those are keyed by
    /// object id, so the replacement makes them unreachable and the
    /// disconnect-time cleanup, which walks the client's object map, no longer
    /// knows they exist.
    ///
    /// The client is sent an error and dropped, so callers only need to stop
    /// what they were doing. Returning `Result` rather than handling it
    /// silently is deliberate: `Result` is `#[must_use]`, so a call site that
    /// forgets to bail out is a compiler warning rather than a latent leak.
    /// A client may only name an id from its own half of the space. The halves
    /// exist so that neither side has to ask the other what is free, and a
    /// client naming one of ours would be handed an id that
    /// [`Self::allocate_id`] later hands out again — quietly replacing the
    /// client's object with a compositor-made one, and stranding whatever
    /// state the first was keyed by. `unregister` would not even tell it,
    /// because a server id is never announced as free.
    pub fn register(&mut self, id: u32, object_type: ObjectType) -> Result<(), ()> {
        if id == 0 || is_server_id(id) {
            tracing::warn!("Client named object id {id}, which is not its to name");
            self.send_error(
                id,
                0,
                &format!("object id {id} is outside the client id space"),
            );
            return Err(());
        }
        if self.objects.contains_key(&id) {
            tracing::warn!("Client reused live object id {}, disconnecting it", id);
            // WL_DISPLAY_ERROR_INVALID_OBJECT = 0
            self.send_error(id, 0, &format!("object id {id} is already in use"));
            return Err(());
        }
        self.objects.insert(id, object_type);
        Ok(())
    }

    /// Take the next id from the compositor's half of the id space and
    /// register an object under it.
    ///
    /// Used where the protocol has the compositor name an object rather than
    /// the client — `zwp_linux_buffer_params_v1.created` carries a `wl_buffer`
    /// the compositor allocates. `None` once the half is exhausted, which takes
    /// sixteen million objects on one connection and is a client with a leak.
    ///
    /// Never reuses an id, which is what makes the bare insert here safe: the
    /// counter only rises, and [`Self::register`] refuses to let a client name
    /// anything in this half, so nothing can already be sitting on the id.
    pub fn allocate_id(&mut self, object_type: ObjectType) -> Option<u32> {
        let id = self.next_server_id;
        if id == u32::MAX {
            tracing::warn!("client has exhausted the server id space");
            return None;
        }
        self.next_server_id += 1;
        self.objects.insert(id, object_type);
        Some(id)
    }

    /// Take the next id from the compositor's half and record a version for it.
    ///
    /// A compositor-named object still has a version, and it is the version of
    /// whatever created it — a `wl_data_offer` speaks the version its
    /// `wl_data_device` was bound at. Without this the object would default to
    /// version 1 and every version-gated event on it would be silently
    /// suppressed, which is a failure that looks exactly like the feature not
    /// being implemented.
    pub fn allocate_id_with_version(
        &mut self,
        object_type: ObjectType,
        version: u32,
    ) -> Option<u32> {
        let id = self.allocate_id(object_type)?;
        self.object_versions.insert(id, version);
        Some(id)
    }

    /// Registers a new wayland object with wayland protocol version information
    pub fn register_with_version(
        &mut self,
        id: u32,
        object_type: ObjectType,
        version: u32,
    ) -> Result<(), ()> {
        self.register(id, object_type)?;
        self.object_versions.insert(id, version);
        Ok(())
    }

    /// Gets the version of an object, defaulting to 1 if the object was registered
    /// without a version #
    pub fn version(&self, id: u32) -> u32 {
        self.object_versions.get(&id).copied().unwrap_or(1)
    }

    /// Unregister a wayland client object
    pub fn unregister(&mut self, id: u32) {
        self.objects.remove(&id);
        self.object_versions.remove(&id);
        // Only the client's own ids are announced: it allocates those, so only
        // it needs telling one is free again. An id from the compositor's half
        // is the compositor's to recycle, and announcing it would invite the
        // client to reuse an id it never owned.
        if is_server_id(id) {
            return;
        }
        let args = ArgWriter::new().u32(id).build();
        let _ = self.send(build_message(
            wl_display::OBJECT_ID,
            wl_display::DELETE_ID,
            args,
        ));
    }

    /// Queue the descriptors one read of this client's socket delivered.
    ///
    /// Called before any of that read's requests are dispatched, which is the
    /// whole of the pairing rule. The kernel releases a `sendmsg`'s descriptors
    /// the moment it copies that `sendmsg`'s first byte, so a descriptor is
    /// never delivered after the request carrying it — only with it, or ahead
    /// of it. Arrival order is therefore enough, and a request claims what it
    /// needs from the front.
    ///
    /// Returns `Err` if the client has gone past [`MAX_PENDING_FDS`] unclaimed
    /// descriptors, having closed the queue and dropped the client. Callers
    /// only need to stop dispatching that read: a client attaching descriptors
    /// to requests that never claim them is not going to say anything worth
    /// hearing afterwards, and the requests in hand were read before it was
    /// cut off.
    pub fn queue_fds(&mut self, fds: Vec<OwnedFd>) -> Result<(), ()> {
        self.fd_queue.extend(fds);
        if self.fd_queue.len() > MAX_PENDING_FDS {
            tracing::warn!(
                "Client has {} unclaimed file descriptors queued (limit {}), disconnecting it",
                self.fd_queue.len(),
                MAX_PENDING_FDS,
            );
            // Closes every one of them.
            self.fd_queue.clear();
            self.cancel_token.cancel();
            return Err(());
        }
        Ok(())
    }

    /// Queue a message for delivery to this client's wayland socket
    ///
    /// This never blocks. The compositor loop is single-threaded and owns all
    /// state, so awaiting a client here would let one client that has stopped
    /// reading its socket stall input, rendering, and every other client. If
    /// the outgoing queue is full we assume the client is wedged and drop it,
    /// which is the same policy libwayland applies once a client's output
    /// buffer grows past its threshold.
    ///
    /// On failure the message is dropped, which closes any file descriptors it
    /// carried — callers do not need to clean them up.
    pub fn send(&self, msg: WaylandEvent) -> Result<(), ()> {
        match self.sender.try_send(msg) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                // Only complain once; later sends land here until the socket
                // tasks notice the cancellation and tear the client down.
                if !self.cancel_token.is_cancelled() {
                    tracing::warn!("Client is not draining its socket, disconnecting it");
                    self.cancel_token.cancel();
                }
                Err(())
            }
            Err(TrySendError::Closed(_)) => {
                tracing::debug!("Client socket closed, dropping outgoing message");
                Err(())
            }
        }
    }

    /// Send a `wl_display.error` to this client and disconnect it.
    ///
    /// There is no other kind. `wl_display.error` is fatal by definition — the
    /// spec has the client disconnect on receiving one, and libwayland
    /// destroys the connection as it posts it — so the disconnect belongs here
    /// rather than at each call site, where it was previously remembered about
    /// four times in thirty-odd. An error the compositor sent and then carried
    /// on from is the worst of both: the client is entitled to consider the
    /// connection dead, while the compositor keeps state for it and keeps
    /// answering requests it may still have in flight.
    ///
    /// A condition the client can recover from is not this. Those are
    /// interface-specific events — `zwp_linux_buffer_params_v1.failed` is one
    /// — and go out as ordinary messages.
    ///
    /// Cancelling stops this client's socket tasks; the read task then reports
    /// the disconnect, and the client's resources are cleaned up through the
    /// same path any other disconnect takes. Callers only need to stop what
    /// they were doing.
    pub fn send_error(&self, object_id: u32, code: u32, msg: &str) {
        let args = ArgWriter::new()
            .u32(object_id)
            .u32(code)
            .string(msg)
            .build();
        let _ = self.send(build_message(
            wl_display::OBJECT_ID,
            wl_display::ERROR,
            args,
        ));
        self.cancel_token.cancel();
    }
}

/// A wrapper for a hashmap of all clients on the compositor keyed by client id
pub struct Clients {
    /// The hashmap
    states: HashMap<u32, ClientState>,
}

impl Clients {
    /// Create a new wrapped client state hashmap
    pub fn new() -> Self {
        Self {
            states: HashMap::new(),
        }
    }

    /// Create/add a new client to hashmap
    pub fn create(
        &mut self,
        client_id: u32,
        sender: Sender<WaylandEvent>,
        cancel_token: CancellationToken,
    ) {
        let client_state = ClientState::new(sender, cancel_token);

        self.states.insert(client_id, client_state);
    }

    /// Get a mutable client state from this hashmap
    pub fn get(&mut self, client_id: u32) -> Option<&mut ClientState> {
        self.states.get_mut(&client_id)
    }

    /// Remove a client
    pub fn remove(&mut self, client_id: u32) {
        self.states.remove(&client_id);
    }

    /// Iterate over all clients
    pub fn iter(&self) -> impl Iterator<Item = (&u32, &ClientState)> {
        self.states.iter()
    }

    /// The version an object was bound at, without borrowing the collection
    /// mutably.
    ///
    /// [`Self::get`] hands back a `&mut ClientState`, which cannot be held
    /// while the rest of `CompositorState` is read. Version gating needs the
    /// answer in exactly that position — deciding whether to send an event,
    /// with the state that decides *what* to send already borrowed.
    pub fn version_of(&self, client_id: u32, object_id: u32) -> Option<u32> {
        self.states
            .get(&client_id)
            .map(|client| client.version(object_id))
    }
}
