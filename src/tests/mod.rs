//! The mechanism test suite, and the shell it drives the mechanism through.
//!
//! These tests came from way-small, where they ran against its real
//! `WaySmallShell` — deliberately, while the crates were one repo: most of
//! what they assert needs *some* policy in place. Living here, they get that
//! policy from [`TestShell`] below instead — the smallest shell that places
//! and stacks windows the way the reference shell does, without workspaces,
//! keybindings, or animations. What those tests assert about the mechanism
//! is unchanged; what they asserted about way-small's own policy (workspace
//! commands, keybinding resolution, fades) stayed in way-small.

mod compositor;
mod socket;

use crate::layer::usable_area;
use crate::shell::Shell;
use crate::state::{ClientObjectId, CompositorState};
use tokio_way_backends::outputs::{Output, OutputId};

/// Per-output window stacks: the minimum model a shell needs to answer the
/// mechanism's queries. One stack per output, bottom to top, plus the windows
/// waiting for an output to exist.
#[derive(Debug, Default)]
pub struct TestStacks {
    stacks: Vec<(OutputId, Vec<ClientObjectId>)>,
    unplaced: Vec<ClientObjectId>,
}

impl TestStacks {
    /// Reconcile the stacks with the outputs that exist: a new output gets a
    /// stack, and the windows of one that has gone come back as unplaced.
    pub fn sync_outputs(&mut self, outputs: &[Output]) -> bool {
        let mut changed = false;
        for output in outputs {
            if !self.stacks.iter().any(|(id, _)| *id == output.id) {
                self.stacks.push((output.id, Vec::new()));
                changed = true;
            }
        }
        let mut orphaned = Vec::new();
        self.stacks.retain(|(id, stack)| {
            if outputs.iter().any(|o| o.id == *id) {
                true
            } else {
                orphaned.extend(stack.iter().copied());
                false
            }
        });
        if !orphaned.is_empty() {
            changed = true;
            self.unplaced.extend(orphaned);
        }
        changed
    }

    /// Which output holds a window.
    pub fn output_of(&self, key: ClientObjectId) -> Option<OutputId> {
        self.stacks
            .iter()
            .find(|(_, stack)| stack.contains(&key))
            .map(|&(id, _)| id)
    }

    /// The stack showing on an output, bottom to top.
    pub fn visible_stack(&self, output: OutputId) -> &[ClientObjectId] {
        self.stacks
            .iter()
            .find(|(id, _)| *id == output)
            .map_or(&[], |(_, stack)| stack.as_slice())
    }

    /// Take a window off whatever stack has it.
    fn remove(&mut self, key: ClientObjectId) {
        for (_, stack) in &mut self.stacks {
            stack.retain(|&k| k != key);
        }
        self.unplaced.retain(|&k| k != key);
    }

    /// Put a window on top of the stack of an output, moving it if placed
    /// elsewhere. False if the output has no stack.
    fn raise_onto(&mut self, output: OutputId, key: ClientObjectId) -> bool {
        if !self.stacks.iter().any(|(id, _)| *id == output) {
            return false;
        }
        self.remove(key);
        let (_, stack) = self
            .stacks
            .iter_mut()
            .find(|(id, _)| *id == output)
            .expect("checked above");
        stack.push(key);
        true
    }

    /// Move a window to the top of the stack it is already in.
    fn raise(&mut self, key: ClientObjectId) {
        for (_, stack) in &mut self.stacks {
            if let Some(index) = stack.iter().position(|&k| k == key) {
                let key = stack.remove(index);
                stack.push(key);
                return;
            }
        }
    }
}

/// The reference shell's placement and stacking, without its policy: every
/// output holds one flat stack, a new window opens at the usable-area origin
/// of the output under the pointer, and a window with no output waits for
/// one. What way-small layers on top — workspaces, keybindings, fades — the
/// mechanism never asks about, so the mechanism tests do not need it.
#[derive(Debug, Default)]
pub struct TestShell {
    /// The per-output stacks. Named `workspaces` so the fixtures shared with
    /// way-small's suite read the same on both sides.
    pub workspaces: TestStacks,
}

impl TestShell {
    /// Place a window on an output: top of its stack, at the usable-area
    /// origin. False if the compositor has not seen that output.
    fn place(
        &mut self,
        state: &mut CompositorState,
        key: ClientObjectId,
        output_id: OutputId,
    ) -> bool {
        let Some(output) = state.outputs.iter().find(|o| o.id == output_id) else {
            return false;
        };
        let (origin_x, origin_y) = (output.geometry.x, output.geometry.y);
        let usable = usable_area(state, output_id);
        if !self.workspaces.raise_onto(output_id, key) {
            return false;
        }
        if let Some(surface) = state.surfaces.get_mut(&key) {
            surface.position = (origin_x + usable.x, origin_y + usable.y);
        }
        true
    }

    /// Move a window to another output's stack. True if it ended up
    /// somewhere new.
    pub fn move_toplevel_to_output(&mut self, key: ClientObjectId, output: OutputId) -> bool {
        if self.workspaces.output_of(key) == Some(output) {
            return false;
        }
        self.workspaces.raise_onto(output, key)
    }

    /// Keep every window on an output that exists, placing the ones that
    /// were waiting for one. True if anything moved.
    pub fn rehome_toplevels(&mut self, state: &mut CompositorState) -> bool {
        let mut moved = self.workspaces.sync_outputs(&state.outputs);
        let unplaced = std::mem::take(&mut self.workspaces.unplaced);
        for key in unplaced {
            let placed = state
                .output_for_new_window()
                .is_some_and(|output| self.place(state, key, output));
            if placed {
                moved = true;
            } else {
                self.workspaces.unplaced.push(key);
            }
        }
        moved
    }

    /// Raise the window a click landed on, with any dialogs of its own. A
    /// layer surface is left alone — where it draws is decided by its layer.
    pub fn raise_window(&mut self, state: &mut CompositorState, surface: ClientObjectId) {
        if state.layer_surface_of(surface).is_some() {
            return;
        }
        match state.toplevel_for_surface(surface) {
            Some(toplevel) => {
                for key in state.toplevel_family(toplevel) {
                    if let Some(surface) = state.surface_of_toplevel(key) {
                        self.workspaces.raise(surface);
                    }
                }
                state.dirty = true;
            }
            None => self.workspaces.raise(surface),
        }
    }
}

impl Shell for TestShell {
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn toplevel_output(&self, key: ClientObjectId) -> Option<OutputId> {
        self.workspaces.output_of(key)
    }

    fn visible_stack(&self, output: OutputId) -> Vec<ClientObjectId> {
        self.workspaces.visible_stack(output).to_vec()
    }

    fn is_visible(&self, key: ClientObjectId) -> bool {
        self.workspaces.output_of(key).is_some()
    }

    fn raise(&mut self, key: ClientObjectId) {
        self.workspaces.raise(key);
    }

    fn toplevel_created(&mut self, state: &mut CompositorState, key: ClientObjectId) {
        let placed = state
            .output_for_new_window()
            .is_some_and(|output| self.place(state, key, output));
        if !placed {
            self.workspaces.unplaced.push(key);
        }
    }

    fn toplevel_destroyed(&mut self, _state: &mut CompositorState, key: ClientObjectId) {
        self.workspaces.remove(key);
    }

    fn client_removed(&mut self, _state: &mut CompositorState, client_id: u32) {
        for (_, stack) in &mut self.workspaces.stacks {
            stack.retain(|&(cid, _)| cid != client_id);
        }
        self.workspaces.unplaced.retain(|&(cid, _)| cid != client_id);
    }

    fn refocus(&mut self, state: &mut CompositorState) {
        let top = state
            .output_for_new_window()
            .and_then(|output| self.workspaces.visible_stack(output).last().copied())
            .or_else(|| {
                self.workspaces
                    .stacks
                    .iter()
                    .find_map(|(_, stack)| stack.last().copied())
            });
        if let Some(key) = top {
            crate::input::switch_focus(state, key);
        }
    }

    fn outputs_changed(&mut self, state: &mut CompositorState) {
        self.workspaces.sync_outputs(&state.outputs);
    }

    fn toplevel_dragged_to_output(
        &mut self,
        _state: &mut CompositorState,
        key: ClientObjectId,
        output: OutputId,
    ) {
        self.move_toplevel_to_output(key, output);
    }

    fn housekeeping(&mut self, state: &mut CompositorState) -> bool {
        self.rehome_toplevels(state)
    }

    /// Clicking a window raises and focuses it, as the reference shell does
    /// — the soak test drives this path with random input.
    fn toplevel_clicked(&mut self, state: &mut CompositorState, hit: &crate::input::HitResult) {
        self.raise_window(state, hit.toplevel);
        state.dirty = true;
        if crate::input::accepts_click_focus(state, hit.toplevel) {
            crate::input::switch_focus(state, hit.toplevel);
        }
    }
}

/// A state wired to the [`TestShell`], as `run_compositor` would build one
/// with a real shell.
pub fn test_state() -> CompositorState {
    CompositorState::with_shell_and_settings(
        crate::Settings::default(),
        Box::new(TestShell::default()),
    )
}

/// The test state's shell, concretely typed, for reaching its stacks.
pub fn ws(state: &mut CompositorState) -> &mut TestShell {
    state
        .shell
        .as_any_mut()
        .downcast_mut()
        .expect("test states are built by test_state(), which uses TestShell")
}

/// Run a closure the way a shell hook runs: shell detached from the state.
pub fn with_ws<R>(
    state: &mut CompositorState,
    f: impl FnOnce(&mut TestShell, &mut CompositorState) -> R,
) -> R {
    state.with_shell(|shell, state| {
        let shell = shell
            .as_any_mut()
            .downcast_mut()
            .expect("test states are built by test_state(), which uses TestShell");
        f(shell, state)
    })
}
