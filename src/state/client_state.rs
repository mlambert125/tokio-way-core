//! Per-client connection state.

use crate::protocol::wire_utils::{ArgWriter, build_message};
use crate::protocol::{ObjectType, wl_display};
use std::collections::{HashMap, VecDeque};
use std::os::fd::OwnedFd;
use tokio::sync::mpsc::{Sender, error::TrySendError};
use tokio_util::sync::CancellationToken;
use tokio_way_sock::WaylandEvent;

/// The most file descriptors a client can have queued but unclaimed before we give up
pub const MAX_PENDING_FDS: usize = 256;

/// The first object id the compositor can hand out.
pub const SERVER_ID_BASE: u32 = 0xff00_0000;

/// Whether an object id belongs to the compositor's half of the id space or a client
pub fn is_server_id(id: u32) -> bool {
    id >= SERVER_ID_BASE
}

/// Client state
pub struct ClientState {
    /// Client created objects (id, type.)  The actual data is stored in compositor state.
    pub objects: HashMap<u32, ObjectType>,
    /// Maps object id to bound interface version for version-gated events
    pub object_versions: HashMap<u32, u32>,
    /// Sender for writing messages back to this client's socket
    pub sender: Sender<WaylandEvent>,
    /// Descriptors this client has sent that no request has claimed yet, in arrival order
    pub fd_queue: VecDeque<OwnedFd>,
    /// Token for canceling this client's socket tasks
    pub cancel_token: CancellationToken,
    /// Next id to hand out from the compositor's half of the id space.
    next_server_id: u32,
}

impl ClientState {
    /// Create a new client state from a `Sender`/`CancellationToken`
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

    /// Register a newly created client wayland object
    ///
    /// # Errors
    ///
    /// Returns `Err` if the client is reusing an id that is still live
    pub fn register_client_object(&mut self, id: u32, object_type: ObjectType) -> Result<(), ()> {
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

    /// Registers a new client wayland object with wayland protocol version information
    pub fn register_client_object_with_version(
        &mut self,
        id: u32,
        object_type: ObjectType,
        version: u32,
    ) -> Result<(), ()> {
        self.register_client_object(id, object_type)?;
        self.object_versions.insert(id, version);
        Ok(())
    }

    /// Registers a newly created server wayland object
    pub fn register_server_object(&mut self, object_type: ObjectType) -> Option<u32> {
        let id = self.next_server_id;
        if id == u32::MAX {
            tracing::warn!("client has exhausted the server id space");
            return None;
        }
        self.next_server_id += 1;
        self.objects.insert(id, object_type);
        Some(id)
    }

    /// Registers a new server wayland object with wayland protocol version information
    pub fn register_server_object_with_version(
        &mut self,
        object_type: ObjectType,
        version: u32,
    ) -> Option<u32> {
        let id = self.register_server_object(object_type)?;
        self.object_versions.insert(id, version);
        Some(id)
    }

    /// Gets the version of an object, defaulting to 1 if the object was registered without
    pub fn version(&self, id: u32) -> u32 {
        self.object_versions.get(&id).copied().unwrap_or(1)
    }

    /// Unregister a wayland client object
    pub fn unregister(&mut self, id: u32) {
        self.objects.remove(&id);
        self.object_versions.remove(&id);
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

    /// Queue the descriptors one read of this client's socket delivered
    ///
    /// # Errors
    ///
    /// Returns `Err` if the client has gone past [`MAX_PENDING_FDS`] unclaimed
    /// descriptors, having closed the queue and dropped the client
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
    pub fn send(&self, msg: WaylandEvent) -> Result<(), ()> {
        match self.sender.try_send(msg) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
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

    /// Send a `wl_display.error` to this client and disconnect it
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
    /// mutably
    pub fn version_of(&self, client_id: u32, object_id: u32) -> Option<u32> {
        self.states
            .get(&client_id)
            .map(|client| client.version(object_id))
    }
}
