//! The compositor event loop, and the frame pacing that hangs off it.
//!
//! One task owns [`CompositorState`] and everything happens to it here, in
//! order: socket messages are dispatched to the protocol handlers, backend
//! messages become input delivery or output bookkeeping, and the bottom of
//! every pass decides what reaches the screen. Policy is asked for through
//! `state.shell` — see [`crate::shell`] for which decisions those are.

use crate::input::{
    self, FrameTarget, Presentation, deliver_pointer_motion, deliver_scroll, deliver_scroll_end,
    dismiss_popups_outside_click, end_grab, finish_buffer_releases, finish_drag,
    fire_frame_callbacks, start_buffer_releases, touch_cancel, touch_down, touch_motion, touch_up,
    update_surface_outputs,
};
use crate::protocol::{self, wl_keyboard, wl_pointer, wl_registry, wl_seat, zwlr_layer_surface};
use crate::scene;
use crate::shell::Shell;
use crate::state::{ClientObjectId, CompositorState};
use crate::{HOUSEKEEPING_INTERVAL, KEY_ESC, Settings};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc::Receiver;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tokio_way_backends::dma::fourcc_name;
use tokio_way_backends::dmabuf_import::DmabufImportProbeResult;
use tokio_way_backends::input::KeyState;
use tokio_way_backends::messages::{BackendMessage, BackendRequest};
use tokio_way_backends::monotonic_timestamp::MonotonicTimeStamp;
use tokio_way_backends::outputs::{Output, OutputId};
use tokio_way_backends::scene_graph::{Scene, SceneGraph};
use tokio_way_sock::{WaylandReadBatch, WaylandRequestWithClientInfo, WaylandSocketMessage};
use tracing::{debug, info, warn};

pub struct FramePacer {
    /// The newest scene composed for each output.
    ///
    /// Kept so that every publication can carry all of them. The frame slot
    /// holds one value and a new publication replaces it, so a frame carrying
    /// only the output being served would drop the scene of an output whose
    /// backend had not drawn it yet.
    pub published: HashMap<OutputId, Arc<Scene>>,
    /// Outputs whose published scene is out of date.
    stale: HashSet<OutputId>,
    /// Outputs whose backend has asked for a frame and not been given one.
    ///
    /// A request outlives the moment it was made. An output that asks while
    /// nothing has changed is not turned away — it is served as soon as
    /// something does, which is what keeps an idle desktop from waiting a
    /// whole refresh period to show the first thing that moves.
    waiting: HashSet<OutputId>,
    /// Source of scene serials. Rises forever, so a backend comparing a scene
    /// against what it last drew on that output cannot be fooled by reuse.
    next_serial: u64,
    /// The cursor as last built, carried into every frame. Rebuilt only when
    /// the pointer moves or its appearance changes, so a scene republish that
    /// nothing to do with the cursor keeps it stable.
    cursor: tokio_way_backends::scene_graph::Cursor,
    /// Source of cursor serials, bumped each time the cursor is rebuilt.
    next_cursor_serial: u64,
    /// Textures kept between scenes.
    cache: scene::SceneCache,
}

impl FramePacer {
    pub fn new() -> Self {
        Self {
            published: HashMap::new(),
            stale: HashSet::new(),
            waiting: HashSet::new(),
            next_serial: 1,
            cursor: tokio_way_backends::scene_graph::Cursor::default(),
            next_cursor_serial: 1,
            cache: scene::SceneCache::new(),
        }
    }

    /// Note that compositor state has moved on, so every output's scene is out
    /// of date.
    ///
    /// State is tracked as one flag rather than per output because almost
    /// everything that changes it — a commit, a focus change, the pointer
    /// moving — could affect any output, and working out which ones it really
    /// touched costs more than composing a scene nobody was waiting for.
    fn invalidate(&mut self, state: &CompositorState) {
        self.stale.extend(state.outputs.iter().map(|o| o.id));
    }

    /// Record that a backend is ready for another frame on this output.
    pub fn request(&mut self, output_id: OutputId) {
        self.waiting.insert(output_id);
    }

    /// Drop cache entries whose reason to exist has passed — see
    /// [`scene::SceneCache::gc`]. Called from housekeeping as well as before
    /// composing, because a retained texture pins a buffer's read guard and
    /// an idle output may not compose for a long time.
    pub fn collect(&mut self, state: &CompositorState) {
        self.cache.gc(state);
    }

    /// Drop everything remembered about outputs that no longer exist.
    pub fn forget_gone_outputs(&mut self, state: &CompositorState) {
        let live: HashSet<OutputId> = state.outputs.iter().map(|o| o.id).collect();
        self.published.retain(|id, _| live.contains(id));
        self.stale.retain(|id| live.contains(id));
        self.waiting.retain(|id| live.contains(id));
    }

    /// Compose for every output that is both waiting and out of date, and
    /// publish the result.
    ///
    /// Never awaited, in line with the rule that the compositor task blocks on
    /// nothing: the slot holds one frame and a backend that has not kept up
    /// gets the newest.
    pub fn publish(&mut self, state: &mut CompositorState, frames: &watch::Sender<SceneGraph>) {
        if state.dirty {
            self.invalidate(state);
            state.dirty = false;
        }

        // The cursor is rebuilt whenever it has moved or changed appearance,
        // whether or not any scene is due. Pointer motion travels this path and
        // no other, so it costs a cursor rebuild rather than a scene recompose.
        let cursor_moved = state.cursor_dirty;
        if cursor_moved {
            self.cursor = scene::build_cursor(state, &mut self.cache);
            self.cursor.serial = self.next_cursor_serial;
            self.next_cursor_serial += 1;
            state.cursor_dirty = false;
        }

        let due: Vec<OutputId> = self.stale.intersection(&self.waiting).copied().collect();
        // Nothing to send unless a scene is owed or the cursor moved. A cursor
        // move alone still republishes — the frame carries the new position —
        // but recomposes no scene.
        if due.is_empty() && !cursor_moved {
            return;
        }

        if !due.is_empty() {
            self.cache.gc(state);
        }
        for &output_id in &due {
            let serial = self.next_serial;
            self.next_serial += 1;
            let mut scene = scene::build(output_id, serial, state, &mut self.cache);
            // What changed since the scene this output last had, named by its
            // serial so a backend can tell whether it drew exactly that one.
            // A backend that did can repaint only these rectangles; one that
            // did not falls back to the whole output, as the empty default says.
            if let Some(previous) = self.published.get(&output_id) {
                scene.damage = scene::output_damage(previous, &scene);
                scene.damage_from = Some(previous.serial);
            }
            self.published.insert(output_id, Arc::new(scene));
            self.stale.remove(&output_id);
            self.waiting.remove(&output_id);
        }

        // Every output draws through one backend and one texture cache, keyed
        // by the buffer's content serial, so a buffer another output has not
        // been composed with yet is either already uploaded or absent — and an
        // absent texture is uploaded whole rather than patched. Carrying the
        // damage forward for that output would therefore change nothing, while
        // an output that stopped asking for frames would grow the list without
        // bound. Only when scenes were actually composed: a cursor-only publish
        // consumed no damage.
        if !due.is_empty() {
            state.clear_buffer_damage();
        }

        let frame = SceneGraph {
            scenes: self.published.values().map(Arc::clone).collect(),
            cursor: self.cursor.clone(),
        };
        drop(frames.send_replace(frame));
    }
}

/// Run the compositor task until the backend closes or the token cancels.
///
/// `settings` are the mechanism's own knobs; `shell` is the window-management
/// policy, asked through the hooks on [`Shell`]. Everything else is the
/// channel wiring to the socket and backend crates.
#[allow(clippy::too_many_lines)]
pub async fn run_compositor(
    settings: Settings,
    shell: Box<dyn Shell>,
    mut wayland_message_receiver: Receiver<WaylandSocketMessage>,
    mut backend_message_receiver: Receiver<BackendMessage>,
    backend_requests: tokio::sync::mpsc::Sender<BackendRequest>,
    frame_sender: watch::Sender<SceneGraph>,
    cancel_token: CancellationToken,
) -> anyhow::Result<()> {
    info!("Running compositor...");

    // Asked once, up front: the answer decides whether clients are offered
    // dma-buf at all, and the backend may not be able to give it until its own
    // display is up. Nothing waits for it — it arrives as a `BackendMessage`.
    if backend_requests
        .try_send(BackendRequest::ProbeDmabuf)
        .is_err()
    {
        warn!("could not ask the backend about dma-buf support");
    }

    let mut state = CompositorState::with_shell_and_settings(settings, shell);
    // Protocol handlers need this too: a client asking for a dma-buf buffer
    // cannot be answered until the backend has tried to import it.
    state.backend_sender = Some(backend_requests.clone());
    state.default_cursor = scene::load_default_cursor();
    if state.default_cursor.is_none() {
        info!("No cursor theme found, using built-in cursor");
    }
    let mut pacer = FramePacer::new();
    // High-water mark for pages the SIGBUS net has blanked, so each new one is
    // reported once.
    let mut reported_patched_pages = 0usize;
    let mut housekeeping_timer = tokio::time::interval(HOUSEKEEPING_INTERVAL);

    // Keys whose press the compositor consumed for a binding. Their release is
    // swallowed too, so a client never sees a release for a key it never saw
    // pressed — which some toolkits treat as a stuck modifier.
    let mut consumed_keys: HashSet<u32> = HashSet::new();

    'compositor: loop {
        // One pass of the loop, whatever happens inside it. The arms below
        // leave early in several places — a key the compositor consumed for a
        // binding, a button that belonged to a grab — and each of those still
        // changed something that has to reach the screen. A bare `continue`
        // would skip the publish at the bottom, which is the one place that
        // decides what any of it looks like, so those become `break 'input`
        // and land there anyway.
        'input: {
            tokio::select! {
                Some(message) = wayland_message_receiver.recv() => {
                    // Process this message, then drain all queued messages before
                    // returning to select. This avoids rendering intermediate states
                    // during bursts of protocol traffic (e.g. client startup).
                    let mut pending = vec![message];
                    while let Ok(msg) = wayland_message_receiver.try_recv() {
                        pending.push(msg);
                    }
                    for message in pending {
                        match message {
                            WaylandSocketMessage::NewClient(msg)  => {
                                info!("New client connected: {}", msg.client_id);
                                state.clients.create(
                                    msg.client_id,
                                    msg.socket_sender,
                                    msg.client_cancel_token,
                                );
                            }
                            WaylandSocketMessage::Read(batch) => {
                                let WaylandReadBatch { client_id, fds, messages } = batch;
                                // The read's descriptors go on the queue before
                                // any of its requests are dispatched. That
                                // ordering is the entire pairing rule — see
                                // `ClientState::queue_fds`.
                                //
                                // A client already gone takes its descriptors
                                // with it: `fds` is dropped here, which closes
                                // them.
                                if let Some(client) = state.clients.get(client_id)
                                    && client.queue_fds(fds).is_err() {
                                        continue;
                                    }
                                for message in messages {
                                    debug!(
                                        "client {}: object_id={} op_code={}",
                                        client_id, message.object_id, message.op_code
                                    );
                                    protocol::handle_message(
                                        &mut state,
                                        &WaylandRequestWithClientInfo { client_id, message },
                                    );
                                }
                            }
                            WaylandSocketMessage::ClientDisconnected { client_id } => {
                                info!("Client {} disconnected", client_id);
                                let was_focused = state.focused_surface.map(|(cid, _)| cid) == Some(client_id);
                                state.remove_client_resources(client_id);
                                state.clients.remove(client_id);
                                state.dirty = true;

                                // Focus went with the client; the shell says
                                // where it lands next.
                                if was_focused {
                                    state.with_shell(super::shell::Shell::refocus);
                                }
                            }
                        }
                    }
                }
                Some(message) = backend_message_receiver.recv() => {
                    match message {
                        BackendMessage::SeatCapabilities { pointer, keyboard, touch } => {
                            info!(
                                "Seat capabilities: pointer={} keyboard={} touch={}",
                                pointer, keyboard, touch
                            );
                            let changed = state.seat.has_pointer != pointer
                                || state.seat.has_keyboard != keyboard
                                || state.seat.has_touch != touch;
                            state.seat.has_pointer = pointer;
                            state.seat.has_keyboard = keyboard;
                            state.seat.has_touch = touch;
                            // A capability can appear after clients have bound the
                            // seat — a touchscreen is only known to exist once it
                            // is touched — so everyone already connected is told
                            // again rather than only whoever binds next.
                            if changed {
                                wl_seat::broadcast_capabilities(&mut state);
                            }
                        }
                        BackendMessage::OutputInfo { outputs } => {
                            let seen: HashSet<OutputId> =
                                outputs.iter().map(|output| output.id).collect();
                            for new_output in outputs {
                                upsert_output(&mut state, new_output);
                            }
                            // `OutputInfo` is the whole set the backend can see, so
                            // an output missing from it has gone. Withdrawing the
                            // global is the half that matters to clients: one that
                            // is announced and never withdrawn leaves them holding a
                            // name for a display that no longer exists.
                            let gone: Vec<OutputId> = state
                                .outputs
                                .iter()
                                .map(|output| output.id)
                                .filter(|id| !seen.contains(id))
                                .collect();
                            for output_id in gone {
                                retire_output(&mut state, output_id);
                            }

                            // A new output arrives meaning nothing to the
                            // shell yet, and a window opening before the shell
                            // has taken it in would have nowhere to go.
                            state.with_shell(super::shell::Shell::outputs_changed);
                        }
                        BackendMessage::OutputAdded { output } => {
                            info!("Output {:?} connected", output.id);
                            upsert_output(&mut state, output);
                            state.with_shell(super::shell::Shell::outputs_changed);
                            state.dirty = true;
                        }
                        BackendMessage::OutputRemoved { output } => {
                            retire_output(&mut state, output);
                            state.with_shell(super::shell::Shell::outputs_changed);
                            state.dirty = true;
                        }
                        BackendMessage::OutputChanged { output } => {
                            let output_id = output.id;
                            info!(
                                "Output {:?} changed: {}x{}",
                                output_id,
                                output.geometry.physical_width,
                                output.geometry.physical_height
                            );
                            upsert_output(&mut state, output);

                            // A window that fit a moment ago can end up entirely
                            // outside a shrunk output, with no edge left to drag
                            // it back by; what to do about that is policy.
                            state.with_shell(|shell, state| shell.output_changed(state, output_id));

                            protocol::wl_output::broadcast_mode(&mut state);
                            state.dirty = true;
                        }
                        BackendMessage::DmabufSupport { formats, probe, device: _ } => {
                            match probe {
                                DmabufImportProbeResult::Passed | DmabufImportProbeResult::Untested(_) => {
                                    info!(
                                        "backend imports dma-buf: {} format(s), e.g. {}",
                                        formats.len(),
                                        describe_formats(formats.iter().take(4)),
                                    );
                                    debug!("dma-buf formats: {}", describe_formats(formats.iter()));
                                }
                                DmabufImportProbeResult::Unsupported(ref reason) => {
                                    info!("backend cannot import dma-buf: {reason}");
                                }
                                DmabufImportProbeResult::Failed(ref reason) => {
                                    warn!("backend dma-buf import is broken: {reason}");
                                }
                            }
                            // Kept for the protocol layer to advertise from. Empty
                            // means no `zwp_linux_dmabuf_v1` global, which is the
                            // honest answer when nothing can be imported.
                            state.dmabuf_formats = formats;

                            // The global appears the moment there is something
                            // behind it, which may be after clients have connected
                            // — hence the broadcast as well as the listing every
                            // later client gets. Guarded so a second answer cannot
                            // advertise the same interface twice.
                            if !state.dmabuf_formats.is_empty()
                                && state.dmabuf_global_name.is_none()
                            {
                                let global_name = state.next_global_number;
                                state.next_global_number += 1;
                                state.dmabuf_global_name = Some(global_name);
                                wl_registry::broadcast_global(
                                    &mut state,
                                    global_name,
                                    protocol::zwp_linux_dmabuf::INTERFACE,
                                    protocol::zwp_linux_dmabuf::VERSION,
                                );
                            }
                        }
                        BackendMessage::DmabufImportResult { token, imported } => {
                            protocol::zwp_linux_buffer_params::resolve_import(
                                &mut state, token, imported,
                            );
                        }
                        BackendMessage::Closed => {
                            info!("Backend requested shutdown");
                            cancel_token.cancel();
                            break 'compositor;
                        }
                        // Nothing here asks for a capture yet, so an answer has
                        // nobody waiting on it.
                        BackendMessage::CaptureResult { token, .. } => {
                            debug!("Unsolicited capture result for token {token}");
                        }
                        // No scene element carries a shader effect yet; if one
                        // ever fails, say so rather than drawing plain silently.
                        BackendMessage::EffectCompileFailed { effect, log } => {
                            warn!("shader effect \"{effect}\" failed to compile: {log}");
                        }
                        BackendMessage::KeyInput { time, keycode, state: key_state } => {
                            let time_ms = time.to_wire_ms();
                            let pressed = matches!(key_state, KeyState::Pressed);
                            let evdev_key = keycode.saturating_sub(8);
                            if pressed {
                                state.pressed_keys.insert(evdev_key);
                            } else {
                                state.pressed_keys.remove(&evdev_key);
                            }

                            // The backend reports which physical key moved; the
                            // modifier masks are the compositor's to derive, from
                            // the same keymap it hands clients — see `Keyboard`.
                            //
                            // Recorded before anything can `continue` past it. A
                            // key the compositor consumes for a binding still
                            // moved the modifier state, and a client that gains
                            // focus afterwards is owed the truth about it — see
                            // `wl_keyboard::send_current_modifiers`.
                            let modifiers = state.keyboard.update_key(keycode, pressed);
                            let modifiers_changed = state.modifiers != modifiers;
                            state.modifiers = modifiers;

                            // Escape abandons a drag. Without it the only way out
                            // of one is to drop it somewhere, and the user may have
                            // no somewhere they are willing to drop it on.
                            if pressed && evdev_key == KEY_ESC && state.drag.is_some() {
                                state.cancel_drag();
                                consumed_keys.insert(evdev_key);
                                break 'input;
                            }

                            // Shell bindings are handled here and never reach the
                            // client, so a bound combination cannot also trigger an
                            // application shortcut.
                            if pressed {
                                if state.with_shell(|shell, state| {
                                    shell.key_pressed(state, evdev_key)
                                }) {
                                    consumed_keys.insert(evdev_key);
                                    break 'input;
                                }
                            } else if consumed_keys.remove(&evdev_key) {
                                break 'input;
                            }

                            // Only send key events to the focused surface's client.
                            //
                            // Modifiers lead the key they belong to, and go out
                            // only when they have actually moved. Both matter: a
                            // client works out what a key press *means* from the
                            // modifier state it holds at the time, so a shift that
                            // arrives after the key it shifted is a modifier the
                            // client applies to the wrong keystroke — and repeating
                            // an unchanged mask on every keystroke is a message per
                            // key saying nothing.
                            let focused_client = state.focused_surface.map(|(cid, _)| cid);
                            for kb in state.keyboards.clone() {
                                if Some(kb.client_id) == focused_client {
                                    if modifiers_changed {
                                        wl_keyboard::send_current_modifiers(&mut state, kb.client_id, kb.object_id);
                                    }
                                    wl_keyboard::send_key(&mut state, kb.client_id, kb.object_id, time_ms, evdev_key, pressed);
                                }
                            }
                        }

                        BackendMessage::MouseMovedTo { time, x, y } => {
                            state.move_cursor_to(x, y);
                            deliver_pointer_motion(&mut state, time.to_wire_ms());
                        }
                        BackendMessage::MouseMovedBy { time, dx, dy } => {
                            state.move_cursor_by(dx, dy);
                            deliver_pointer_motion(&mut state, time.to_wire_ms());
                        }
                        BackendMessage::MouseButton { time, button, state: btn_state } => {
                            let time_ms = time.to_wire_ms();
                            let pressed = matches!(btn_state, tokio_way_backends::input::ButtonState::Pressed);
                            // The wire wants the evdev code, which the button
                            // already is.
                            let linux_button = button.evdev_code();

                            if pressed {
                                state.pressed_buttons.insert(linux_button);
                            } else {
                                state.pressed_buttons.remove(&linux_button);
                            }

                            // A grab runs until the button comes up, and swallows
                            // everything in between.
                            if state.pointer_grab.is_some() {
                                if !pressed {
                                    end_grab(&mut state);
                                }
                                break 'input;
                            }

                            // So does a drag, and for the same reason: the
                            // compositor owns the pointer, and what the button does
                            // is decide how the drag ends.
                            if state.drag.is_some() {
                                if !pressed {
                                    finish_drag(&mut state);
                                }
                                break 'input;
                            }

                            // Moves or resizes without the client's help, or any
                            // other button policy the shell has: asked with the
                            // hit already tested, since the shell cannot hit-test
                            // while it is detached from the state.
                            let hit = pressed
                                .then(|| input::hit_test(&state, state.cursor_x, state.cursor_y))
                                .flatten();
                            if pressed
                                && state.with_shell(|shell, state| {
                                    shell.pointer_pressed(state, button, hit.as_ref())
                                })
                            {
                                break 'input;
                            }

                            // Dismiss grabbed popups if click lands outside them
                            if pressed && !state.grabbed_popups.is_empty() {
                                let dismissed = dismiss_popups_outside_click(&mut state);
                                if dismissed {
                                    // Don't process the click further — it was consumed by dismissal
                                    break 'input;
                                }
                            }

                            // On press, hit-test again — a dismissal above may
                            // have changed what is under the cursor — and let
                            // the shell raise and focus what was clicked. The
                            // guard is mechanism: only a press that would move
                            // focus is a click the shell needs to hear about.
                            let cx = state.cursor_x;
                            let cy = state.cursor_y;
                            if pressed && let Some(hit) = input::hit_test(&state, cx, cy) && state.focused_surface != Some(hit.toplevel) {
                                state.with_shell(|shell, state| shell.toplevel_clicked(state, &hit));

                                // Update pointer surface to the specific surface under cursor
                                if state.pointer_surface != Some(hit.surface) {
                                    state.pointer_surface = Some(hit.surface);
                                    let local_x = cx - f64::from(hit.surface_x);
                                    let local_y = cy - f64::from(hit.surface_y);
                                    for ptr in state.pointers.clone() {
                                        if ptr.client_id == hit.surface.0 {
                                            wl_pointer::send_enter(&mut state, ptr.client_id, ptr.object_id, hit.surface.1, local_x, local_y);
                                            wl_pointer::send_frame(&mut state, ptr.client_id, ptr.object_id);
                                        }
                                    }
                                }
                            }

                            // Send button event to the pointer surface
                            if let Some(ps) = state.pointer_surface {
                                for ptr in state.pointers.clone() {
                                    if ptr.client_id == ps.0 {
                                        let serial = wl_pointer::send_button(&mut state, ptr.client_id, ptr.object_id, time_ms, linux_button, pressed);
                                        // Remembered so the client can quote it if
                                        // this press turns into a move or resize.
                                        if pressed {
                                            state.last_button_serial.insert(ptr.client_id, serial);
                                        }
                                        wl_pointer::send_frame(&mut state, ptr.client_id, ptr.object_id);
                                    }
                                }
                            }
                        }
                        BackendMessage::TouchDown { time, id, x, y } => {
                            touch_down(&mut state, time.to_wire_ms(), id, x, y);
                        }
                        BackendMessage::TouchMotion { time, id, x, y } => {
                            touch_motion(&mut state, time.to_wire_ms(), id, x, y);
                        }
                        BackendMessage::TouchUp { time, id } => {
                            touch_up(&mut state, time.to_wire_ms(), id);
                        }
                        BackendMessage::TouchCancel => {
                            touch_cancel(&mut state);
                        }
                        BackendMessage::MouseScroll { time, dx, dy, source, v120_x, v120_y } => {
                            deliver_scroll(&mut state, time.to_wire_ms(), dx, dy, source, v120_x, v120_y);
                        }
                        BackendMessage::MouseScrollEnd { time } => {
                            deliver_scroll_end(&mut state, time.to_wire_ms());
                        }
                        BackendMessage::FocusIn => {
                            debug!("Focus in");
                        }
                        BackendMessage::FocusOut => {
                            debug!("Focus out");
                        }
                        BackendMessage::FrameRequested { output, predicted_present, refresh_ns } => {
                            // The backend can show another frame here. Whether one
                            // gets composed is settled at the bottom of the loop,
                            // where both halves of that decision are in view.
                            let _ = refresh_ns;
                            // An animation composes for the instant this frame
                            // will reach the screen, which is what makes it
                            // vsync-paced: present → next request → next step.
                            if state.with_shell(|shell, state| shell.frame(state, predicted_present)) {
                                state.dirty = true;
                            }
                            pacer.request(output);
                        }
                        BackendMessage::FramePresented { output, time, refresh_ns, sequence, flags } => {
                            // Clients pace themselves on this, so it follows the
                            // frame reaching the screen rather than the compositor
                            // handing it over. Callbacks live in surface state
                            // until fired, so a frame the backend skipped costs a
                            // client latency but never a lost callback.
                            //
                            // Only the surfaces this output was showing: a client
                            // with a window on each of two displays is paced by
                            // each of them separately, which is the whole point of
                            // the output being named here.
                            fire_frame_callbacks(
                                &mut state,
                                time.to_wire_ms(),
                                FrameTarget::Output(output),
                                Some(Presentation { at: time, refresh_ns, sequence, flags }),
                            );
                        }
                    }
                }
                _ = housekeeping_timer.tick() => {
                    update_surface_outputs(&mut state);
                    // Policy that has to notice what no event announces — a
                    // window left homeless by an output that vanished.
                    if state.with_shell(super::shell::Shell::housekeeping) {
                        state.dirty = true;
                    }
                    // Animations normally advance on `FrameRequested`'s clock;
                    // this is the fallback that keeps one moving under a
                    // backend that never asks for frames.
                    if state.with_shell(|shell, state| shell.frame(state, MonotonicTimeStamp::now()))
                    {
                        state.dirty = true;
                    }
                    pacer.forget_gone_outputs(&state);
                    // A bell that has run its course has to be taken off the screen,
                    // and nothing else would notice it had expired.
                    state.expire_bells();

                    // Surfaces no display is showing are paced from here, because
                    // nothing else will pace them: no output will ever report
                    // presenting them. Their presentation feedback is `discarded`
                    // rather than `presented`, which is what it is — nothing
                    // reached a screen.
                    let timestamp_ms = MonotonicTimeStamp::now().to_wire_ms();
                    fire_frame_callbacks(&mut state, timestamp_ms, FrameTarget::Offscreen, None);

                    // Both independent of the frame path: a buffer becomes free
                    // when the last frame referencing it goes, which can happen on
                    // a tick where nothing has changed. The cache is collected
                    // first for the same reason — a retained-content entry whose
                    // gap has closed holds a read guard, and an idle output may
                    // not compose (and so not collect) for a long time.
                    pacer.collect(&state);
                    start_buffer_releases(&mut state);
                    finish_buffer_releases(&mut state);

                    // The SIGBUS handler cannot log, so the count is reported here.
                    let patched = tokio_way_backends::shm::patched_pages();
                    if patched > reported_patched_pages {
                        warn!(
                            "{} page(s) of client shm blanked after a pool was truncated \
                             while in use; that client is showing black where it shrank",
                            patched - reported_patched_pages
                        );
                        reported_patched_pages = patched;
                    }
                }
                () = cancel_token.cancelled() => {
                    info!("Compositor received shutdown signal");
                    break 'compositor;
                }
            }
        }

        // The shell asks to exit through state rather than a channel of its
        // own — see `CompositorState::exit_requested`. Checked once per pass,
        // after whatever arm ran its hooks, and it takes the whole process
        // down the same way the backend closing does.
        if state.exit_requested {
            info!("Shell requested exit");
            cancel_token.cancel();
            break 'compositor;
        }

        // Composing happens here rather than in any one arm. An output is owed
        // a scene when it is both waiting for one and out of date, and those
        // two halves are settled by different messages arriving at different
        // times — a page flip on one side, a client commit on the other. Asking
        // once per pass is what keeps the answer from depending on which of
        // them happened to arrive second.
        pacer.publish(&mut state, &frame_sender);
    }

    Ok(())
}

/// Take an output as the backend describes it into compositor state: update
/// the one already known under its id, or add and advertise a new one.
///
/// The settings' `output_scale` wins over what the backend reports, which is
/// what lets a scaled desktop be tried on hardware that is not scaled.
fn upsert_output(state: &mut CompositorState, mut output: Output) {
    if let Some(scale) = state.settings.output_scale {
        output.scale = tokio_way_backends::outputs::Scale::from_f64(scale);
    }
    if let Some(existing) = state.outputs.iter_mut().find(|o| o.id == output.id) {
        // Update in place, preserving the global name mapping.
        existing.geometry = output.geometry;
        existing.modes = output.modes;
        existing.scale = output.scale;
        existing.description = output.description;
    } else {
        // New output — assign a global name and advertise
        let global_name = state.next_global_number;
        state.next_global_number += 1;
        state.output_global_names.insert(output.id, global_name);
        state.outputs.push(output);
        wl_registry::broadcast_global(
            state,
            global_name,
            protocol::WL_OUTPUT_INTERFACE,
            protocol::WL_OUTPUT_VERSION,
        );
    }
}

/// Retire an output that has gone: close the layer surfaces anchored to it
/// and withdraw its global.
///
/// Withdrawing the global is the half that matters to clients: one that is
/// announced and never withdrawn leaves them holding a name for a display
/// that no longer exists.
fn retire_output(state: &mut CompositorState, output_id: OutputId) {
    info!("Output {:?} disconnected", output_id);
    // A panel anchored to a display that has gone has nowhere to be. The
    // protocol's answer is to tell it so and let it destroy itself, rather
    // than leaving it holding a surface placed against nothing.
    let orphaned: Vec<ClientObjectId> = state
        .layer_surfaces
        .iter()
        .filter(|(_, layer)| layer.output == Some(output_id))
        .map(|(&key, _)| key)
        .collect();
    for key in orphaned {
        zwlr_layer_surface::send_closed(state, key);
    }
    if let Some(global_name) = state.remove_output(output_id) {
        wl_registry::broadcast_global_remove(state, global_name);
    }
}

/// A short, loggable summary of a dma-buf format list.
fn describe_formats<'a>(
    formats: impl Iterator<Item = &'a tokio_way_backends::dma::DmabufFormat>,
) -> String {
    formats
        .map(|f| {
            format!(
                "{} ({} modifier(s))",
                fourcc_name(f.fourcc),
                f.modifiers.len()
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}
