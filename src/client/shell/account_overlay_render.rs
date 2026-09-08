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
    if let Some(question) = picker.confirm.as_deref() {
        return render_switch_confirm(b, picker, question, p);
    }
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
    let count = match (picker.interrupt(), filtered.len() == picker.entries.len()) {
        // The switch's one knob, always visible so its state is never a
        // surprise when the confirmation warns about an interrupt.
        (Some(true), _) => "i: interrupt if working — on".to_owned(),
        (Some(false), _) => "i: interrupt if working — off".to_owned(),
        (None, true) => format!("{} accounts", picker.entries.len()),
        (None, false) => format!("{}/{} accounts", filtered.len(), picker.entries.len()),
    };
    put_right_text(
        b,
        search,
        search.y,
        &count,
        Style::default().fg(p.overlay0).bg(p.panel_bg),
    );
    // Everything below the filter line is laid out from the panel's bottom
    // up — the button row, then the status line — and the list takes what is
    // left. On a terminal too small for that, each part is skipped rather
    // than drawn over the part below it or outside the panel: `popup` only
    // guarantees an inner height of two.
    if inner.height >= 3 {
        put_text(
            b,
            inner.x,
            inner.y + 2,
            inner.width,
            &"─".repeat(inner.width as usize),
            Style::default().fg(p.surface1).bg(p.panel_bg),
        );
    }

    let body = Rect::new(
        inner.x,
        inner.y.saturating_add(3),
        inner.width,
        inner.height.saturating_sub(7),
    );
    let visible_count = (body.height / 2) as usize;
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
    if filtered.is_empty() && visible_count > 0 {
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
    if inner.height < 6 {
        // No room between the filter line and the buttons.
    } else if let Some(progress) = picker.progress {
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

    let settled = picker.settled;
    // Wide enough for the switch's longer verb; the start picker keeps the
    // width every other herdr modal uses.
    let primary_width = if picker.interrupt().is_some() { 19 } else { 16 };
    let buttons = row(
        inner,
        &[primary_width, 12],
        2,
        inner.height.saturating_sub(1),
    );
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
        } else if picker.interrupt().is_some() {
            " ↵ switch account "
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
            x: search
                .x
                .saturating_add(3)
                .saturating_add(display_width(&picker.query))
                .min(search.right().saturating_sub(1)),
            y: search.y,
            visible: true,
            shape: 0,
        }),
        ..OverlayRender::default()
    })
}

/// The switch's confirmation: the machine's own question, and two answers.
///
/// Drawn like `ClientConfirmCloseOverlay` — a red panel, a body, `confirm` and
/// `cancel` — because it is the same kind of decision: something the user owns
/// is about to be stopped and started again. The body is
/// `SwitchMachine`'s text verbatim, so what the modal promises and what the
/// protocol does cannot drift apart.
///
/// No row or search rectangle goes out: while the question is up, the only two
/// things on the modal that do anything are its buttons.
fn render_switch_confirm(
    b: &mut Buffer,
    picker: &ClientAccountPickerOverlay,
    question: &str,
    p: &Palette,
) -> Option<OverlayRender> {
    let width = 72_u16;
    let lines = wrap_question(question, width.saturating_sub(4) as usize);
    let height = (lines.len() as u16).saturating_add(6).clamp(8, 24);
    let popup = popup(b.area, width, height)?;
    let inner = panel(b, popup, p.red, p.panel_bg)?;
    put_text(
        b,
        inner.x,
        inner.y,
        inner.width,
        &format!(" {}", picker.title()),
        Style::default()
            .fg(p.red)
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
    // The body stops where the button row starts, so a question longer than
    // the panel is truncated rather than drawn over the answers.
    let body_rows = inner.height.saturating_sub(3);
    for (index, line) in lines.iter().take(body_rows as usize).enumerate() {
        put_text(
            b,
            inner.x,
            inner.y.saturating_add(2).saturating_add(index as u16),
            inner.width,
            &format!(" {line}"),
            Style::default().fg(p.text).bg(p.panel_bg),
        );
    }

    let buttons = row(inner, &[13, 12], 2, inner.height.saturating_sub(1));
    let [ok, cancel] = buttons.as_slice() else {
        return None;
    };
    button(
        b,
        *ok,
        " ↵ confirm ",
        Style::default()
            .fg(contrast(p))
            .bg(p.red)
            .add_modifier(Modifier::BOLD),
    );
    button(
        b,
        *cancel,
        " esc cancel ",
        Style::default()
            .fg(p.text)
            .bg(p.surface0)
            .add_modifier(Modifier::BOLD),
    );
    Some(OverlayRender {
        primary: *ok,
        clear: Rect::default(),
        cancel: *cancel,
        worktree_search: Rect::default(),
        worktree_rows: Vec::new(),
        cursor: None,
        ..OverlayRender::default()
    })
}

/// Break the machine's question into panel-width lines.
///
/// It arrives as a few `\n`-separated sentences, some of them indented
/// continuations, and each can be far wider than the panel. Words are never
/// split; a word longer than the width is put on its own line and truncated by
/// `put_text` rather than silently dropped.
fn wrap_question(question: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for paragraph in question.split('\n') {
        let indent = if paragraph.starts_with(' ') { "  " } else { "" };
        let mut current = String::new();
        for word in paragraph.split_whitespace() {
            let candidate = if current.is_empty() {
                format!("{indent}{word}")
            } else {
                format!("{current} {word}")
            };
            if display_width(&candidate) as usize > width && !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current = format!("{indent}{word}");
            } else {
                current = candidate;
            }
        }
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use account_overlay::{AccountEntry, PickerMode};

    fn palette() -> Palette {
        crate::app::client_palette_from_config(&crate::config::Config::default())
    }

    fn entry(name: &str) -> AccountEntry {
        AccountEntry {
            name: name.to_owned(),
            config_dir: format!("/tmp/profiles/{name}"),
            dir_exists: true,
            logged_in: true,
            hook_installed: true,
            is_default: name == "work",
            is_current: false,
        }
    }

    fn picker(count: usize) -> ClientAccountPickerOverlay {
        ClientAccountPickerOverlay::idle(
            "pane_2".to_owned(),
            PickerMode::Start {
                agent_name: "claude".to_owned(),
            },
            (0..count)
                .map(|index| entry(&format!("acct-{index}")))
                .collect(),
            0,
        )
    }

    #[test]
    fn every_terminal_size_renders_without_panicking() {
        let palette = palette();
        for cols in 1..=40_u16 {
            for rows in 1..=14_u16 {
                let mut buffer = Buffer::empty(Rect::new(0, 0, cols, rows));
                let idle = picker(3);
                let _ = render_account_picker(&mut buffer, &idle, &palette);

                let mut settled = picker(3);
                settled.settled = true;
                settled.error = Some("claude did not start: agent_pane_busy".to_owned());
                settled.warnings = vec!["CLAUDE_CONFIG_DIR is still exported".to_owned()];
                settled.search_focused = true;
                settled.query = "a".repeat(200);
                let _ = render_account_picker(&mut buffer, &settled, &palette);

                let (running, _tx) = picker(3).test_running(std::time::Instant::now());
                let _ = render_account_picker(&mut buffer, &running, &palette);
            }
        }
    }

    #[test]
    fn a_full_size_render_reports_one_hit_per_visible_row_and_nothing_while_running() {
        let palette = palette();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 40));
        let idle = picker(3);
        let rendered = render_account_picker(&mut buffer, &idle, &palette).expect("rendered");
        assert_eq!(rendered.worktree_rows.len(), 3);
        let indices = rendered
            .worktree_rows
            .iter()
            .map(|(_, index)| *index)
            .collect::<Vec<_>>();
        assert_eq!(indices, vec![0, 1, 2]);
        assert!(!rendered.worktree_search.is_empty());
        assert!(!rendered.primary.is_empty());
        assert!(!rendered.cancel.is_empty());
        assert!(
            rendered.cursor.is_none(),
            "no cursor until the filter is focused"
        );

        let (running, _tx) = picker(3).test_running(std::time::Instant::now());
        let rendered = render_account_picker(&mut buffer, &running, &palette).expect("rendered");
        assert!(
            rendered.worktree_rows.is_empty(),
            "no row may be clicked mid-launch"
        );
        assert!(rendered.worktree_search.is_empty());
        assert!(rendered.cancel.is_empty(), "no cancel mid-launch");
        assert!(rendered.cursor.is_none());
    }

    #[test]
    fn the_filter_narrows_the_hits_to_the_matching_rows() {
        let palette = palette();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 40));
        let mut filtered = picker(3);
        filtered.query = "acct-2".to_owned();
        filtered.search_focused = true;
        let rendered = render_account_picker(&mut buffer, &filtered, &palette).expect("rendered");
        assert_eq!(
            rendered
                .worktree_rows
                .iter()
                .map(|(_, index)| *index)
                .collect::<Vec<_>>(),
            vec![2]
        );
        let cursor = rendered.cursor.expect("the filter has the cursor");
        assert_eq!(cursor.y, rendered.worktree_search.y);
        assert!(cursor.x < rendered.worktree_search.right());
    }

    fn switch_picker() -> ClientAccountPickerOverlay {
        ClientAccountPickerOverlay::idle(
            "pane_2".to_owned(),
            PickerMode::Switch {
                agent_name: "a1".to_owned(),
                current: Some("acct-0".to_owned()),
                interrupt: false,
            },
            (0..2)
                .map(|index| entry(&format!("acct-{index}")))
                .collect(),
            1,
        )
    }

    #[test]
    fn the_confirmation_offers_two_answers_and_nothing_to_click_past_them() {
        let palette = palette();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 40));
        let mut picker = switch_picker();
        picker.confirm = Some(
            "Switch agent \"a1\" in pane pane_2 from perso to \"work\"?\n  Claude will be \
             asked to exit (Escape if needed, then /exit) and started again with `--resume \
             abc-123` under the new profile."
                .to_owned(),
        );
        let rendered = render_account_picker(&mut buffer, &picker, &palette).expect("rendered");
        assert!(!rendered.primary.is_empty(), "confirm");
        assert!(!rendered.cancel.is_empty(), "cancel");
        assert!(
            rendered.worktree_rows.is_empty() && rendered.worktree_search.is_empty(),
            "no row or filter may be clicked while the question is up"
        );
        assert!(rendered.cursor.is_none());

        let drawn = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        // The three facts the user is answering about are on screen.
        assert!(drawn.contains("switch claude account"), "{drawn}");
        assert!(drawn.contains("pane pane_2"), "{drawn}");
        assert!(drawn.contains("abc-123"), "{drawn}");
        assert!(
            drawn.contains("confirm") && drawn.contains("cancel"),
            "{drawn}"
        );
    }

    #[test]
    fn every_terminal_size_renders_the_confirmation_without_panicking() {
        let palette = palette();
        for cols in 1..=40_u16 {
            for rows in 1..=14_u16 {
                let mut buffer = Buffer::empty(Rect::new(0, 0, cols, rows));
                let mut picker = switch_picker();
                picker.confirm = Some(format!(
                    "Switch agent \"a1\"?\n  {}\n  {}",
                    "x".repeat(300),
                    "word ".repeat(80)
                ));
                let _ = render_account_picker(&mut buffer, &picker, &palette);
            }
        }
    }

    #[test]
    fn the_switch_picker_shows_the_interrupt_toggle_and_its_own_button() {
        let palette = palette();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 40));
        let drawn = |picker: &ClientAccountPickerOverlay, buffer: &mut Buffer| {
            render_account_picker(buffer, picker, &palette).expect("rendered");
            (0..buffer.area.height)
                .map(|y| {
                    (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let text = drawn(&switch_picker(), &mut buffer);
        assert!(text.contains("interrupt if working — off"), "{text}");
        assert!(text.contains("switch account"), "{text}");

        let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 40));
        let mut interrupting = switch_picker();
        if let PickerMode::Switch { interrupt, .. } = &mut interrupting.mode {
            *interrupt = true;
        }
        let text = drawn(&interrupting, &mut buffer);
        assert!(text.contains("interrupt if working — on"), "{text}");

        // The start picker keeps the account count and its own verb.
        let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 40));
        let text = drawn(&picker(3), &mut buffer);
        assert!(text.contains("3 accounts"), "{text}");
        assert!(text.contains("start claude"), "{text}");
    }

    #[test]
    fn wrapping_never_splits_a_word_and_never_drops_one() {
        let wrapped = wrap_question("alpha beta gamma delta", 11);
        assert_eq!(wrapped, vec!["alpha beta", "gamma delta"]);
        // An indented continuation keeps its indent on every line it takes.
        let wrapped = wrap_question("head\n  one two three", 9);
        assert_eq!(wrapped, vec!["head", "  one two", "  three"]);
        // A word wider than the panel gets a line of its own rather than
        // pushing the rest off the end.
        let wrapped = wrap_question("short verylongwordindeed tail", 8);
        assert_eq!(wrapped, vec!["short", "verylongwordindeed", "tail"]);
        // Every word survives, whatever the width.
        for width in 1..=40 {
            let joined = wrap_question("a bb ccc dddd eeeee", width).join(" ");
            assert_eq!(
                joined.split_whitespace().collect::<Vec<_>>(),
                vec!["a", "bb", "ccc", "dddd", "eeeee"],
                "width {width}"
            );
        }
    }

    #[test]
    fn a_long_list_scrolls_to_keep_the_selection_visible() {
        let palette = palette();
        // 20 entries at two rows each will not fit a 26-row popup.
        let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 30));
        let mut long = picker(20);
        long.selected = 19;
        let rendered = render_account_picker(&mut buffer, &long, &palette).expect("rendered");
        assert!(
            rendered.worktree_rows.iter().any(|(_, index)| *index == 19),
            "the selected row must be among the drawn ones: {:?}",
            rendered.worktree_rows
        );
        assert!(rendered.worktree_rows.len() < 20);
        for (rect, _) in &rendered.worktree_rows {
            assert!(rect.bottom() <= buffer.area.bottom());
        }
    }
}
