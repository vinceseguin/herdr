//! Fork (E9): drawing for the account picker.
//!
//! A child of `overlays` because `panel`, `popup`, `row`, `button` and
//! `contrast` — the shared modal chrome every herdr dialog is built from — are
//! private to it, exactly as `worktree_overlays` and `settings_overlay` are.
//! The state, the input routing and the launch job live in the sibling
//! fork module `crate::client::shell::account_overlay`; this file only draws,
//! and mutates nothing.

use super::*;

use account_overlay::ClientAccountPickerOverlay;

/// The picker, laid out like `open worktree`: a filter line, a list of
/// two-line rows, one status line, and the shared button row.
///
/// The row and search hit rectangles go out through `OverlayRender`'s
/// `worktree_*` fields, which `composition.rs` copies into the hit map for
/// every overlay it renders. Reusing them keeps this feature to one added
/// arm per upstream file; the mouse router reads them only while the account
/// picker is the open overlay, and the worktree routers only while a worktree
/// overlay is, so the two never see each other's rectangles.
pub(super) fn render_account_picker(
    b: &mut Buffer,
    picker: &ClientAccountPickerOverlay,
    p: &Palette,
) -> Option<OverlayRender> {
    let popup_height = (picker.entries.len().saturating_mul(2) + 9).clamp(14, 26) as u16;
    let popup = popup(b.area, 84, popup_height)?;
    let inner = panel(b, popup, p.accent, p.panel_bg)?;
    let running = picker.running();
    put_text(
        b,
        inner.x,
        inner.y,
        inner.width,
        picker.title(),
        Style::default()
            .fg(p.text)
            .bg(p.panel_bg)
            .add_modifier(Modifier::BOLD),
    );
    put_right_text(
        b,
        inner,
        inner.y,
        &format!("pane {}", picker.pane_id),
        Style::default().fg(p.overlay0).bg(p.panel_bg),
    );

    let search = Rect::new(inner.x, inner.y + 1, inner.width, 1);
    let filtered = picker.filtered_indices();
    put_text(
        b,
        search.x,
        search.y,
        search.width,
        &if picker.search_focused || !picker.query.is_empty() {
            format!(" / {}", picker.query)
        } else {
            " / filter accounts".to_owned()
        },
        Style::default()
            .fg(if picker.search_focused {
                p.text
            } else {
                p.overlay0
            })
            .bg(p.panel_bg),
    );
    let count = if filtered.len() == picker.entries.len() {
        format!("{} accounts", picker.entries.len())
    } else {
        format!("{}/{} accounts", filtered.len(), picker.entries.len())
    };
    put_right_text(
        b,
        search,
        search.y,
        &count,
        Style::default().fg(p.overlay0).bg(p.panel_bg),
    );
    put_text(
        b,
        inner.x,
        inner.y + 2,
        inner.width,
        &"─".repeat(inner.width as usize),
        Style::default().fg(p.surface1).bg(p.panel_bg),
    );

    let body = Rect::new(
        inner.x,
        inner.y + 3,
        inner.width,
        inner.height.saturating_sub(7),
    );
    let visible_count = (body.height / 2).max(1) as usize;
    let selected_index = picker.selected_entry_index();
    let selected_position = selected_index
        .and_then(|selected| filtered.iter().position(|index| *index == selected))
        .unwrap_or(0);
    let start = selected_position
        .saturating_sub(visible_count.saturating_sub(1))
        .min(filtered.len().saturating_sub(visible_count));
    let mut row_hits = Vec::new();
    for (visible, entry_index) in filtered
        .iter()
        .copied()
        .skip(start)
        .take(visible_count)
        .enumerate()
    {
        let Some(entry) = picker.entries.get(entry_index) else {
            continue;
        };
        let rect = Rect::new(body.x, body.y + visible as u16 * 2, body.width, 2);
        row_hits.push((rect, entry_index));
        let selected = Some(entry_index) == selected_index;
        let style = if selected {
            Style::default().fg(contrast(p)).bg(p.accent)
        } else {
            Style::default().fg(p.text).bg(p.panel_bg)
        };
        b.set_style(rect, style);
        put_text(
            b,
            rect.x,
            rect.y,
            rect.width,
            &format!(" {}", entry.name),
            style.add_modifier(Modifier::BOLD),
        );
        let status = entry.status_text();
        if !status.is_empty() {
            put_right_text(
                b,
                rect,
                rect.y,
                &status,
                if selected {
                    style
                } else if entry.unusable() {
                    Style::default().fg(p.red).bg(p.panel_bg)
                } else {
                    Style::default().fg(p.overlay0).bg(p.panel_bg)
                },
            );
        }
        put_text(
            b,
            rect.x,
            rect.y + 1,
            rect.width,
            &format!(" {}", entry.config_dir),
            if selected {
                style
            } else {
                Style::default().fg(p.overlay0).bg(p.panel_bg)
            },
        );
    }
    if filtered.is_empty() {
        put_text(
            b,
            body.x,
            body.y,
            body.width,
            " no matching accounts",
            Style::default().fg(p.overlay0).bg(p.panel_bg),
        );
    }

    // One status line: the phase while a launch runs, then whatever the user
    // has to read afterwards. A failure wins over a warning.
    let status_y = inner.bottom().saturating_sub(3);
    if let Some(progress) = picker.progress {
        put_text(
            b,
            inner.x,
            status_y,
            inner.width,
            &format!(" {progress}"),
            Style::default().fg(p.accent).bg(p.panel_bg),
        );
    } else if let Some(error) = picker.error.as_deref() {
        put_text(
            b,
            inner.x,
            status_y,
            inner.width,
            &format!(" {error}"),
            Style::default().fg(p.red).bg(p.panel_bg),
        );
    } else if let Some(warning) = picker.warnings.first() {
        put_text(
            b,
            inner.x,
            status_y,
            inner.width,
            &format!(" {warning}"),
            Style::default().fg(p.yellow).bg(p.panel_bg),
        );
    }

    let settled = picker.error.is_some() || !picker.warnings.is_empty();
    let buttons = row(inner, &[16, 12], 2, inner.height.saturating_sub(1));
    let [primary, cancel] = buttons.as_slice() else {
        return None;
    };
    button(
        b,
        *primary,
        if running {
            " working... "
        } else if settled {
            " ↵ close "
        } else {
            " ↵ start claude "
        },
        Style::default()
            .fg(contrast(p))
            .bg(if running { p.overlay0 } else { p.accent })
            .add_modifier(Modifier::BOLD),
    );
    if !running {
        button(
            b,
            *cancel,
            " esc cancel ",
            Style::default()
                .fg(p.text)
                .bg(p.surface0)
                .add_modifier(Modifier::BOLD),
        );
    }
    Some(OverlayRender {
        primary: *primary,
        clear: Rect::default(),
        cancel: if running { Rect::default() } else { *cancel },
        worktree_search: if running { Rect::default() } else { search },
        worktree_rows: if running { Vec::new() } else { row_hits },
        cursor: (picker.search_focused && !running).then(|| crate::protocol::CursorState {
            x: (search.x + 3 + display_width(&picker.query)).min(search.right().saturating_sub(1)),
            y: search.y,
            visible: true,
            shape: 0,
        }),
        ..OverlayRender::default()
    })
}
