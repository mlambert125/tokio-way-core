//! A compositor on a real socket, and a client to talk to it.
//!
//! Everything else in this test suite calls `handle_message` directly, which
//! is fast, deterministic, and blind to an entire subsystem: the socket task,
//! the send task, message framing over a stream that splits wherever it likes,
//! and descriptor passing as ancillary data. Those are exactly the places
//! where a fault shows up far from its cause — a mispaired descriptor corrupts
//! every later one on that connection — and none of them can be reached
//! without a socket.
//!
//! So this starts the real subsystems over a real Unix socket in a temporary
//! directory and connects to them the way a client would. No backend: the
//! channels are created and nobody drives them, which is enough for anything
//! at protocol level and keeps the tests off the GPU.
//!
//! Everything here is bounded by a timeout. A test that hangs waiting for an
//! event that will never arrive tells you nothing and blocks CI; one that
//! fails after a second names the event it wanted.

use crate::Settings;
use crate::tests::TestShell;
use sendfd::SendWithFd;
use std::collections::VecDeque;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::UnixStream;
use tokio::sync::{mpsc::channel, watch};
use tokio_util::sync::CancellationToken;
use tokio_way_backends::messages::{BackendMessage, BackendRequest};
use tokio_way_backends::scene_graph::SceneGraph;
use tokio_way_core::protocol::wire_utils::ArgWriter;
use tokio_way_sock::{WaylandRequest, WaylandSocketMessage};

/// What came of trying to take a message off the front of a read buffer.
///
/// The socket library frames requests this way internally but does not export
/// its framer, so the client half of the harness carries its own — the wire
/// format is the fixed 8-byte Wayland header, not an implementation detail.
enum Framed {
    /// A whole message, now removed from the buffer.
    Message(WaylandRequest),
    /// Not all of one has arrived yet. The buffer is left alone.
    Incomplete,
    /// The header says the message is shorter than a header, so there is no
    /// way to find where the next one begins. Carries the length claimed.
    Malformed(u16),
}

/// Take the next complete message off the front of a read buffer.
fn take_message(data: &mut VecDeque<u8>) -> Framed {
    if data.len() < 8 {
        return Framed::Incomplete;
    }
    let object_id = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let length_and_opcode = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    let message_length = (length_and_opcode >> 16) as u16;
    let op_code = (length_and_opcode & 0xFFFF) as u16;

    if message_length < 8 {
        return Framed::Malformed(message_length);
    }
    if data.len() < message_length as usize {
        return Framed::Incomplete;
    }

    let mut message = data.drain(..message_length as usize);
    message.by_ref().take(8).for_each(drop);

    Framed::Message(WaylandRequest {
        object_id,
        op_code,
        args: message.collect(),
    })
}

/// How long any single wait is given before the test gives up.
///
/// Generous for a local socket and short enough that a wedged test fails
/// rather than hanging a CI run.
const TIMEOUT: Duration = Duration::from_secs(5);

/// Distinguishes concurrent tests' socket paths. Tests share a process, and
/// two of them binding the same path would fight over the lock file.
static NEXT_HARNESS: AtomicU32 = AtomicU32::new(0);

/// A running compositor: the socket and compositor tasks, and the token that
/// stops them.
pub struct Harness {
    pub socket_path: PathBuf,
    directory: PathBuf,
    cancel: CancellationToken,
}

impl Harness {
    /// Start the socket and compositor subsystems on a socket of their own.
    pub async fn start() -> Self {
        let id = NEXT_HARNESS.fetch_add(1, Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("way-small-test-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let socket_path = directory.join("wayland-test");

        let cancel = CancellationToken::new();

        let (wayland_tx, wayland_rx) = channel::<WaylandSocketMessage>(1000);
        // Nothing drives the backend side. The compositor asks about dma-buf
        // on startup and never hears back, which is the same position it is in
        // under a backend that cannot import: no global is advertised, and
        // every protocol path that does not need a display works exactly as it
        // would with one.
        let (backend_tx, backend_rx) = channel::<BackendMessage>(1000);
        let (request_tx, _request_rx) = channel::<BackendRequest>(64);
        let (frame_tx, _frame_rx) = watch::channel::<SceneGraph>(SceneGraph::default());
        drop(backend_tx);

        tokio::spawn(tokio_way_core::run_compositor(
            Settings::default(),
            Box::new(TestShell::default()),
            wayland_rx,
            backend_rx,
            request_tx,
            frame_tx,
            cancel.clone(),
        ));
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<String>();
        tokio::spawn(tokio_way_sock::run_wayland_socket(
            Some(socket_path.to_str().unwrap().to_string()),
            ready_tx,
            wayland_tx,
            cancel.clone(),
        ));

        // The listener binds inside its task, so a client connecting straight
        // away could beat it to the socket. The socket says when it is
        // listening — the same signal the startup programs wait on — which is
        // exact where watching for the path to appear was merely close: the
        // file exists between `bind` and `listen`, and a connection in that
        // window is refused rather than queued.
        tokio::time::timeout(TIMEOUT, ready_rx)
            .await
            .expect("the compositor never bound its socket")
            .expect("the socket task died before it was listening");

        Self {
            socket_path,
            directory,
            cancel,
        }
    }

    /// Connect a client, the way a real one would.
    pub async fn connect(&self) -> Client {
        let stream = tokio::time::timeout(TIMEOUT, UnixStream::connect(&self.socket_path))
            .await
            .expect("timed out connecting")
            .expect("could not connect");
        Client {
            stream,
            buffered: VecDeque::new(),
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.cancel.cancel();
        std::fs::remove_dir_all(&self.directory).ok();
    }
}

/// One connection to the compositor, speaking the wire protocol.
pub struct Client {
    stream: UnixStream,
    /// Bytes read but not yet framed. A read returns whatever was in the
    /// socket, so an event can arrive in pieces or several can arrive at once.
    buffered: VecDeque<u8>,
}

impl Client {
    /// Send a request.
    pub async fn send(&mut self, object_id: u32, op_code: u16, args: &[u8]) {
        self.send_with_fds(object_id, op_code, args, &[]).await;
    }

    /// Write bytes straight onto the socket, framed however the caller likes.
    ///
    /// For the tests that are about framing itself: a request split one byte
    /// per write, or several packed into one. Everything else should go
    /// through [`Self::send`], which builds a well-formed message.
    pub async fn send_raw(&mut self, bytes: &[u8]) {
        let mut sent = 0;
        while sent < bytes.len() {
            self.stream.writable().await.expect("socket not writable");
            match self.stream.try_write(&bytes[sent..]) {
                Ok(n) => sent += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("write failed: {e}"),
            }
        }
    }

    /// Write raw bytes with descriptors attached to this write alone.
    ///
    /// [`Self::send_raw`] with ancillary data, for the one thing neither it nor
    /// [`Self::send_with_fds`] can script: a `sendmsg` that carries a
    /// descriptor and stops partway through the request that claims it.
    pub async fn send_raw_with_fds(&mut self, bytes: &[u8], fds: &[RawFd]) {
        let mut sent = 0;
        while sent < bytes.len() {
            self.stream.writable().await.expect("socket not writable");
            let to_send = if sent == 0 { fds } else { &[] };
            match self.stream.try_io(tokio::io::Interest::WRITABLE, || {
                self.stream.send_with_fd(&bytes[sent..], to_send)
            }) {
                Ok(n) => sent += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("send failed: {e}"),
            }
        }
    }

    /// Send a request carrying descriptors as ancillary data.
    ///
    /// This is the path nothing else in the suite can exercise: the compositor
    /// receives these out of band and pairs them with messages by counting,
    /// which is only correct if both sides agree about which requests carry
    /// one.
    pub async fn send_with_fds(
        &mut self,
        object_id: u32,
        op_code: u16,
        args: &[u8],
        fds: &[RawFd],
    ) {
        let length = u32::try_from(args.len() + 8).unwrap();
        let mut message = Vec::with_capacity(args.len() + 8);
        message.extend_from_slice(&object_id.to_le_bytes());
        message.extend_from_slice(&((length << 16) | u32::from(op_code)).to_le_bytes());
        message.extend_from_slice(args);

        let mut sent = 0;
        while sent < message.len() {
            self.stream.writable().await.expect("socket not writable");
            let to_send = if sent == 0 { fds } else { &[] };
            match self.stream.try_io(tokio::io::Interest::WRITABLE, || {
                self.stream.send_with_fd(&message[sent..], to_send)
            }) {
                Ok(n) => sent += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("send failed: {e}"),
            }
        }
    }

    /// The next event, or `None` if the compositor closed the connection.
    ///
    /// Framed with the compositor's own `take_message`, so the client agrees
    /// with the server about where a message ends by construction rather than
    /// by a second implementation that could disagree.
    pub async fn next_event(&mut self) -> Option<WaylandRequest> {
        loop {
            match take_message(&mut self.buffered) {
                Framed::Message(message) => return Some(message),
                Framed::Malformed(length) => {
                    panic!("compositor sent a message claiming {length} bytes")
                }
                Framed::Incomplete => {}
            }

            let mut buffer = [0u8; 4096];
            let read = tokio::time::timeout(TIMEOUT, self.stream.read(&mut buffer))
                .await
                .expect("timed out waiting for an event")
                .expect("read failed");
            if read == 0 {
                return None;
            }
            self.buffered.extend(&buffer[..read]);
        }
    }

    /// Read events until one satisfies `wanted`, returning it.
    ///
    /// Panics on end-of-stream, naming what it was waiting for — which is far
    /// more use than a timeout when the compositor has dropped the connection
    /// rather than gone quiet.
    pub async fn wait_for(
        &mut self,
        what: &str,
        mut wanted: impl FnMut(&WaylandRequest) -> bool,
    ) -> WaylandRequest {
        let mut seen = Vec::new();
        loop {
            let Some(event) = self.next_event().await else {
                panic!("connection closed while waiting for {what}; saw {seen:?}");
            };
            if wanted(&event) {
                return event;
            }
            seen.push((event.object_id, event.op_code));
        }
    }

    /// Collect every event already available, without waiting for more.
    ///
    /// For asserting that something did *not* arrive, and for draining the
    /// burst a request kicks off before looking for what comes next.
    pub async fn drain(&mut self) -> Vec<WaylandRequest> {
        let mut events = Vec::new();
        loop {
            // Frame whatever is already buffered first.
            if let Framed::Message(message) = take_message(&mut self.buffered) {
                events.push(message);
                continue;
            }
            let mut buffer = [0u8; 4096];
            match tokio::time::timeout(Duration::from_millis(50), self.stream.read(&mut buffer))
                .await
            {
                Ok(Ok(0)) | Err(_) => return events,
                Ok(Ok(read)) => self.buffered.extend(&buffer[..read]),
                Ok(Err(e)) => panic!("read failed: {e}"),
            }
        }
    }

    /// Wait for the connection to be closed, and say whether it was.
    pub async fn wait_for_close(&mut self) -> bool {
        loop {
            match tokio::time::timeout(TIMEOUT, self.next_event()).await {
                Ok(None) => return true,
                Ok(Some(_)) => {}
                Err(_) => return false,
            }
        }
    }
}

/// A memfd of `size` bytes, for the requests that carry a descriptor.
pub fn memfd(size: u64) -> OwnedFd {
    // SAFETY: `memfd_create` takes a name and flags and returns a new fd.
    let fd = unsafe { libc::memfd_create(c"harness".as_ptr().cast(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0, "memfd_create failed");
    let file = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd) };
    file.set_len(size).unwrap();
    OwnedFd::from(file)
}

/// The raw descriptor, for handing to `send_with_fds`.
pub fn raw(fd: &OwnedFd) -> RawFd {
    fd.as_raw_fd()
}

/// Argument bytes, so tests read as the protocol does.
pub fn args() -> ArgWriter {
    ArgWriter::new()
}
