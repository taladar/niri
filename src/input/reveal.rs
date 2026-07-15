//! Non-focusing "show" actions.
//!
//! These actions change what a *non-focused* output displays — which window is
//! scrolled into view, which workspace is active, or the horizontal scroll
//! position — without moving the real keyboard focus.
//!
//! In niri the keyboard focus follows the active monitor: [`crate::niri::Niri`]
//! recomputes the focused surface from the active monitor's active
//! workspace/column/window every refresh (see
//! `crate::niri::Niri::update_keyboard_focus`). Consequently these actions only
//! ever act on an output that is *not* the one holding keyboard focus. Touching
//! the focused output's workspace/scroll would move focus and could scroll the
//! focused window off screen; acting on a different output leaves the focused
//! window's output, workspace and scroll position untouched, so the focused
//! window is never moved off screen. Each action therefore refuses (with a
//! warning) when its target resolves to the focused output.

use niri_config::WorkspaceReference;
use smithay::output::Output;

use crate::niri::State;

impl State {
    /// Show a window on its output without focusing it: make its workspace the
    /// active one on its output and scroll the window into view. No-op if the
    /// window does not exist or is on the focused output.
    pub fn show_window(&mut self, id: u64) {
        let found = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
        let Some((Some(mon), m)) = found else {
            warn!("cannot show window: no window with id {id}");
            return;
        };
        let output = mon.output().clone();
        let window = m.window.clone();

        if self.niri.layout.active_output() == Some(&output) {
            warn!("refusing to show window {id}: it is on the focused output");
            return;
        }

        self.niri.layout.show_window(&window);
        self.niri.queue_redraw(&output);
    }

    /// Show a workspace on its output without focusing it. No-op if the
    /// reference is unknown or resolves to the focused output.
    pub fn show_workspace(&mut self, reference: WorkspaceReference) {
        let Some((output, index)) = self.niri.find_output_and_workspace_index(reference) else {
            warn!("cannot show workspace: unknown reference");
            return;
        };

        // `output == None` means the reference (e.g. a bare index) resolves to the
        // focused output, which we refuse.
        let Some(output) = output else {
            warn!("refusing to show workspace: it is on the focused output");
            return;
        };
        if self.niri.layout.active_output() == Some(&output) {
            warn!("refusing to show workspace: it is on the focused output");
            return;
        }

        if let Some(mon) = self.niri.layout.monitor_for_output_mut(&output) {
            mon.switch_workspace(index);
            self.niri.queue_redraw(&output);
        }
    }

    pub fn show_column_left(&mut self, output: Option<String>) {
        self.show_column(output, false);
    }

    pub fn show_column_right(&mut self, output: Option<String>) {
        self.show_column(output, true);
    }

    fn show_column(&mut self, output: Option<String>, right: bool) {
        let Some(output) = self.resolve_show_output(output) else {
            return;
        };
        if let Some(mon) = self.niri.layout.monitor_for_output_mut(&output) {
            let ws = mon.active_workspace();
            if right {
                ws.focus_right();
            } else {
                ws.focus_left();
            }
            self.niri.queue_redraw(&output);
        }
    }

    pub fn show_workspace_up(&mut self, output: Option<String>) {
        self.show_workspace_switch(output, false);
    }

    pub fn show_workspace_down(&mut self, output: Option<String>) {
        self.show_workspace_switch(output, true);
    }

    fn show_workspace_switch(&mut self, output: Option<String>, down: bool) {
        let Some(output) = self.resolve_show_output(output) else {
            return;
        };
        if let Some(mon) = self.niri.layout.monitor_for_output_mut(&output) {
            if down {
                mon.switch_workspace_down();
            } else {
                mon.switch_workspace_up();
            }
            self.niri.queue_redraw(&output);
        }
    }

    /// Resolve which output a directional `show-*` action should act on. With an
    /// explicit name, look it up; otherwise pick the unique output that is not
    /// the focused one. Returns `None` (after warning) if the output cannot be
    /// determined unambiguously or would be the focused output.
    fn resolve_show_output(&self, output: Option<String>) -> Option<Output> {
        let active = self.niri.layout.active_output().cloned();

        match output {
            Some(name) => {
                let Some(out) = self.niri.output_by_name_match(&name).cloned() else {
                    warn!("cannot show: no output matching {name:?}");
                    return None;
                };
                if active.as_ref() == Some(&out) {
                    warn!("refusing to show on {name:?}: it is the focused output");
                    return None;
                }
                Some(out)
            }
            None => {
                let mut others = self
                    .niri
                    .layout
                    .outputs()
                    .filter(|o| active.as_ref() != Some(*o))
                    .cloned();
                let first = others.next();
                if others.next().is_some() {
                    warn!("cannot show: more than one non-focused output, specify --output");
                    return None;
                }
                if first.is_none() {
                    warn!("cannot show: no non-focused output");
                }
                first
            }
        }
    }
}
