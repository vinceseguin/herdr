//! The Fleet console's host picker overlay (fork).
//!
//! `prefix+shift+f` (`[fleet.keys] host_picker`) lists every configured host
//! with its state and counts; `enter`, a click or `1`-`9` then `enter` makes
//! one of them the routing target. The overlay follows the navigator's
//! pattern — a list, `↑↓`/`j k`, `enter`, `esc`, no dimmed background — rather
//! than inventing a screen of its own.
//!
//! Three rules keep a picker from typing into the wrong machine:
//!
//! * A row is only ever accepted through
//!   [`ClientShellState::switch_action`], the same guard the sidebar's
//!   click path uses: the host that is already active and a host `[fleet]`
//!   disables are not switch targets, so the picker cannot emit an action the
//!   loop would refuse.
//! * A click is resolved against rows this picker drew. Opening clears the
//!   picker's hit rects and the previous overlay's primary rect, so a press
//!   that lands in the same input batch as the opening key — before any frame
//!   has drawn the picker — cannot borrow another overlay's geometry, and a
//!   rect that names no current row is ignored rather than accepting whichever
//!   row happens to be highlighted.
//! * Accepting closes the overlay before the switch is dispatched, and a
//!   switch resets the shell anyway ([`ClientShellState::reset_for_host_switch`]),
//!   so no row of the machine the console just left survives the switch.
//!
//! E2 PR 6. The rows themselves are pure — `crate::fleet::sidebar` builds them
//! once per fleet change, and the overlay only ever reads them.

use super::*;

impl ClientShellState {
    /// Open the host picker, if this client is a console.
    ///
    /// Returns whether it opened: outside fleet mode there is no fleet to pick
    /// from, and the action is a logged no-op rather than an empty overlay.
    pub(super) fn open_host_picker_overlay(&mut self) -> bool {
        let Some(fleet) = self.fleet.as_ref() else {
            tracing::debug!("host picker outside fleet mode");
            return false;
        };
        let rows = fleet.model.picker.clone();
        if rows.is_empty() {
            tracing::debug!("host picker with no configured hosts");
            return false;
        }
        let selected = rows.iter().position(|row| row.active).unwrap_or(0);
        // The hit map describes the last frame drawn, which may be a previous
        // picker at another size or another overlay entirely. A press that
        // arrives before this picker's first frame must find no row and no
        // popup, so it closes the picker instead of acting on stale geometry.
        self.hits.host_picker_rows.clear();
        self.hits.overlay_primary = ratatui::layout::Rect::default();
        self.overlay = Some(ClientShellOverlay::HostPicker(ClientHostPickerOverlay {
            rows,
            selected,
            scroll: 0,
        }));
        true
    }

    /// Refresh an open picker from the console's current row model.
    ///
    /// Called when the loop installs a rebuilt model, so a host that drops
    /// while the picker is open is redrawn as unavailable instead of staying
    /// `connected` until the overlay is reopened. The highlight stays on the
    /// same *host*, found by id: row order is `[fleet]` order and does not
    /// move today, but the selection must not silently land on a different
    /// machine if it ever does. Costs nothing when no picker is open.
    pub(super) fn refresh_host_picker_rows(&mut self) {
        let Some(ClientShellOverlay::HostPicker(picker)) = self.overlay.as_mut() else {
            return;
        };
        let Some(fleet) = self.fleet.as_ref() else {
            return;
        };
        let rows = &fleet.model.picker;
        let selected_host = picker.rows.get(picker.selected).map(|row| &row.host);
        picker.selected = selected_host
            .and_then(|host| rows.iter().position(|row| &row.host == host))
            .unwrap_or_else(|| picker.selected.min(rows.len().saturating_sub(1)));
        picker.rows.clone_from(rows);
    }

    /// Move the selection by `delta` rows, clamped to the list.
    ///
    /// `isize::MIN` and `isize::MAX` are valid deltas (Home and End).
    pub(super) fn move_host_picker_selection(&mut self, delta: isize) {
        let Some(ClientShellOverlay::HostPicker(picker)) = self.overlay.as_mut() else {
            return;
        };
        let last = picker.rows.len().saturating_sub(1);
        picker.selected = picker.selected.saturating_add_signed(delta).min(last);
    }

    /// Select one row by index; out-of-range indices (a `1`-`9` key with fewer
    /// hosts than that, or a hit rect from an older frame) leave the
    /// selection alone and return `false`.
    pub(super) fn select_host_picker_row(&mut self, index: usize) -> bool {
        let Some(ClientShellOverlay::HostPicker(picker)) = self.overlay.as_mut() else {
            return false;
        };
        if index >= picker.rows.len() {
            return false;
        }
        picker.selected = index;
        true
    }

    /// Select `index` and accept it — a click on a drawn row.
    ///
    /// The two steps are one call so that a rect which no longer names a row
    /// can never fall through to [`Self::accept_host_picker`] and switch to
    /// whichever row was highlighted before the click.
    pub(super) fn accept_host_picker_row(&mut self, index: usize, outcome: &mut ClientShellInput) {
        if self.select_host_picker_row(index) {
            self.accept_host_picker(outcome);
        }
    }

    /// Switch to the selected host, or do nothing if it cannot be switched to.
    ///
    /// A disabled host keeps the picker open — it is drawn dim, and closing on
    /// a click that does nothing reads as a lost keystroke. The host that is
    /// already active closes it: the user asked to be where they already are.
    pub(super) fn accept_host_picker(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::HostPicker(picker)) = self.overlay.as_ref() else {
            return;
        };
        let Some(row) = picker.rows.get(picker.selected) else {
            return;
        };
        if !row.enabled {
            tracing::debug!(host = %row.host, "host picker row for a host disabled in [fleet]");
            return;
        }
        let host = row.host.clone();
        let active = row.active;
        self.overlay = None;
        outcome.repaint = true;
        if active {
            return;
        }
        // The same guard the sidebar's click path uses, so the picker cannot
        // hand the loop a switch it would only log and drop.
        if let Some(action) = self.switch_action(host, None) {
            outcome.actions.push(ClientShellAction::Fleet(action));
        }
    }

    /// The picker row a point lands on, if any.
    pub(super) fn host_picker_row_at(&self, point: (u16, u16)) -> Option<usize> {
        self.hits
            .host_picker_rows
            .iter()
            .find(|(rect, _)| super::contains(*rect, point))
            .map(|(_, index)| *index)
    }
}
