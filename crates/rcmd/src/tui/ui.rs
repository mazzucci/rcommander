//! Rendering. Colors use the 256-color palette because macOS Terminal.app has no truecolor.

use super::app::{App, EDITOR_ROW, HIDDEN_ROW, LEFT, Mode, Options};
use super::views::{DiffView, Viewer, sanitize};
use chrono::{Local, Offset, TimeZone};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Paragraph, Row as TableRow, Table};
use rc_core::Settings;
use rc_core::diff::{DiffRow, RowKind, Side, Visible};
use rc_core::dircmp::CmpMark;
use rc_core::format::human_size;
use rc_core::{Column, Panel, Row, SortKey};

// Palette (256-color indices so it also works in macOS Terminal.app).
const ACCENT: Color = Color::Indexed(75);
const TEXT: Color = Color::Indexed(252);
const META: Color = Color::Indexed(246);
const DIM: Color = Color::Indexed(244);
const RULE: Color = Color::Indexed(238);
const DIR: Color = Color::Indexed(111);
const SELECTED: Color = Color::Indexed(221);
const CURSOR_ACTIVE: Color = Color::Indexed(24);
const CURSOR_INACTIVE: Color = Color::Indexed(236);
const OK: Color = Color::Indexed(114);
const ERR: Color = Color::Indexed(203);
const DEL_BG: Color = Color::Indexed(52);
const DEL_STRONG: Color = Color::Indexed(124);
const INS_BG: Color = Color::Indexed(22);
const INS_STRONG: Color = Color::Indexed(28);
const HATCH: Color = Color::Indexed(237);

pub fn draw(f: &mut Frame, app: &mut App) {
    let [main, message, keys] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1), Constraint::Length(1)]).areas(f.area());

    match &mut app.mode {
        Mode::Viewer(v) => draw_viewer(f, main, v),
        Mode::Diff(d) => draw_diff(f, main, d),
        _ => {
            let [l, r] = Layout::horizontal([Constraint::Percentage(50); 2]).areas(main);
            draw_panel(f, l, app, LEFT);
            draw_panel(f, r, app, 1);
        }
    }

    draw_message(f, message, app);
    draw_keys(f, keys, &app.mode);

    match &app.mode {
        Mode::Prompt(p) => {
            let area = popup(main, 70, 5);
            // Keep the end of long input (where the user types) visible.
            let input = format!("{}█", elide_start(&p.input, area.width.saturating_sub(3) as usize));
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(ACCENT))
                .title(Line::raw(format!(" {} ", p.title)).fg(TEXT).bold());
            f.render_widget(Clear, area);
            f.render_widget(
                Paragraph::new(vec![Line::raw(""), Line::raw(input).white()]).block(block),
                area,
            );
        }
        Mode::Confirm(c) => {
            let area = popup(main, 60, 5);
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(RULE))
                .title(Line::raw(" Confirm ").fg(ERR).bold());
            let yes = if c.enter_confirms { "Enter/y" } else { "y" };
            let text = vec![
                Line::raw(c.text.as_str()).fg(TEXT),
                Line::raw(""),
                Line::from(vec![
                    Span::raw(yes).fg(ACCENT).bold(),
                    Span::raw(" yes    ").fg(DIM),
                    Span::raw("Esc/n").fg(ACCENT).bold(),
                    Span::raw(" no").fg(DIM),
                ]),
            ];
            f.render_widget(Clear, area);
            f.render_widget(Paragraph::new(text).alignment(Alignment::Center).block(block), area);
        }
        Mode::Help => draw_help(f, main),
        Mode::Options(o) => draw_options(f, main, o, &app.settings),
        _ => {}
    }
}

fn popup(area: Rect, width_pct: u16, height: u16) -> Rect {
    let w = ((u32::from(area.width) * u32::from(width_pct) / 100) as u16)
        .max(30)
        .min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 3,
        width: w,
        height: h,
    }
}

/// Keep the left end readable by eliding from the start: "…/very/long/path".
fn elide_start(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max || max < 2 {
        return s.to_string();
    }
    let tail: String = s.chars().skip(n - (max - 1)).collect();
    format!("…{tail}")
}

/// Local UTC offset in seconds at a given moment (DST-aware).
fn utc_offset(secs: i64) -> i64 {
    Local
        .timestamp_opt(secs, 0)
        .single()
        .map_or(0, |d| d.offset().fix().local_minus_utc().into())
}

fn mark_color(m: CmpMark) -> Color {
    Color::Indexed(m.color().1)
}

/// Bottom line of a panel. Only what differs from the defaults is mentioned.
fn panel_status(p: &Panel, settings: &Settings) -> String {
    let (n, size) = p.selection_summary();
    let visible = p.rows().iter().filter(|r| matches!(r, Row::Entry(_))).count();
    let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    let mut s = match (n, size) {
        (0, _) => format!(" {} ", plural(visible, "item")),
        // Only directories selected: their size is unknown, so don't print "0".
        (n, 0) => format!(" {n} of {visible} selected "),
        (n, size) => format!(" {n} of {visible} selected · {} ", human_size(size)),
    };
    if p.sort != SortKey::Name || p.sort_desc {
        s += &format!("· {} {} ", p.sort.label(), if p.sort_desc { "↓" } else { "↑" });
    }
    if !p.filter.is_empty() {
        s += &format!("· filter “{}” ", p.filter);
    }
    if p.show_hidden != settings.show_hidden {
        s += if p.show_hidden {
            "· hidden shown "
        } else {
            "· hidden off "
        };
    }
    s
}

fn draw_panel(f: &mut Frame, area: Rect, app: &mut App, side: usize) {
    let is_active = side == app.active;
    let filtering = is_active && matches!(app.mode, Mode::Filter);
    let p = &app.panels[side];
    let border = if is_active { ACCENT } else { RULE };
    let title = elide_start(&format!(" {} ", p.location), area.width.saturating_sub(4) as usize);
    let status = if filtering {
        format!(" filter: {}█ ", p.filter)
    } else {
        panel_status(p, &app.settings)
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border))
        .title(Line::raw(title).bold().fg(if is_active { Color::White } else { DIM }))
        .title_bottom(Line::raw(status).fg(if filtering { SELECTED } else { DIM }));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [header_area, rule_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)]).areas(inner);

    // Keep the cursor inside the visible window.
    let height = list_area.height as usize;
    app.page = height.saturating_sub(1).max(1);
    let scroll = &mut app.scroll[side];
    if p.cursor < *scroll {
        *scroll = p.cursor;
    } else if height > 0 && p.cursor >= *scroll + height {
        *scroll = p.cursor + 1 - height;
    }
    let scroll = *scroll;

    // Optional columns that fit while leaving the name at least 12 characters.
    let columns = app.settings.fit_columns(inner.width, 12, 2);

    let cursor_bg = if is_active { CURSOR_ACTIVE } else { CURSOR_INACTIVE };
    let cell = |text: String, c: Column| {
        let line = Line::raw(text);
        Cell::from(if c.right_aligned() {
            line.alignment(Alignment::Right)
        } else {
            line
        })
    };
    let rows = (scroll..p.len().min(scroll + height)).map(|r| {
        let (mark, name, values, mut style) = match p.rows()[r] {
            Row::Parent => (
                Cell::from(" "),
                "..".to_string(),
                columns.iter().map(|&c| cell(String::new(), c)).collect::<Vec<_>>(),
                Style::new().fg(DIR).bold(),
            ),
            Row::Entry(_) => {
                let e = p.row_entry(r).expect("entry row");
                let mark = p.marks.get(&e.name).copied();
                let mut style = if e.is_dir {
                    Style::new().fg(DIR).bold()
                } else {
                    Style::new().fg(TEXT)
                };
                if e.hidden {
                    style = style.fg(DIM);
                }
                if let Some(m) = mark.filter(|m| *m != CmpMark::Same) {
                    style = style.fg(mark_color(m));
                }
                if p.is_selected(e) {
                    style = style.fg(SELECTED).bold();
                }
                let mut name = if e.is_dir {
                    format!("{}/", e.name)
                } else {
                    e.name.clone()
                };
                if e.is_link {
                    name += &format!(" → {}", e.link_target.as_deref().unwrap_or("?"));
                }
                let mark = mark.map_or(Cell::from(" "), |m| {
                    Cell::from(m.symbol().to_string()).style(Style::new().fg(mark_color(m)))
                });
                // Metadata stays quiet so names stand out (selection still colors it).
                let selected = p.is_selected(e);
                let values = columns
                    .iter()
                    .map(|&c| {
                        let v = cell(c.text(e, utc_offset), c);
                        if selected { v } else { v.style(Style::new().fg(META)) }
                    })
                    .collect();
                (mark, name, values, style)
            }
        };
        if r == p.cursor {
            // Keep selected entries recognizable under the cursor.
            let selected = p.row_entry(r).is_some_and(|e| p.is_selected(e));
            let fg = if selected { SELECTED } else { Color::White };
            style = style.bg(cursor_bg).fg(fg);
            if is_active {
                style = style.add_modifier(Modifier::BOLD);
            }
        }
        let cells = [mark, Cell::from(name)].into_iter().chain(values);
        TableRow::new(cells).style(style)
    });

    let widths = [Constraint::Length(1), Constraint::Fill(1)]
        .into_iter()
        .chain(columns.iter().map(|c| Constraint::Length(c.width())));
    let header = ["".to_string(), "Name".to_string()]
        .into_iter()
        .chain(columns.iter().map(|c| {
            if c.right_aligned() {
                format!("{:>w$}", c.title(), w = c.width() as usize)
            } else {
                c.title().to_string()
            }
        }));
    let widths: Vec<Constraint> = widths.collect();
    let no_rows: [TableRow; 0] = [];
    let header = Table::new(no_rows, widths.clone())
        .header(TableRow::new(header).style(Style::new().fg(DIM)))
        .column_spacing(1);
    f.render_widget(header, header_area);
    f.render_widget(Paragraph::new("─".repeat(rule_area.width as usize)).fg(RULE), rule_area);
    let only_parent = p.rows().iter().all(|r| matches!(r, Row::Parent));
    if only_parent && list_area.height > 2 {
        let text = if p.filter.is_empty() {
            "Empty folder · F7 new folder"
        } else {
            "Nothing matches the filter"
        };
        let [_, middle, _] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1), Constraint::Fill(1)]).areas(list_area);
        f.render_widget(Paragraph::new(text).fg(DIM).alignment(Alignment::Center), middle);
    }
    f.render_widget(Table::new(rows, widths).column_spacing(1), list_area);
}

fn draw_message(f: &mut Frame, area: Rect, app: &App) {
    let line = if let Some(job) = app.job_status() {
        Line::raw(format!(" ⏳ {job}")).fg(SELECTED)
    } else if let Some((msg, is_err)) = &app.message {
        Line::raw(format!(" {msg}")).fg(if *is_err { ERR } else { OK })
    } else {
        Line::raw(" ? help · o options").fg(DIM)
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_keys(f: &mut Frame, area: Rect, mode: &Mode) {
    let keys: &[(&str, &str)] = match mode {
        Mode::Viewer(_) => &[("Esc", "Close"), ("↑↓", "Scroll"), ("←→", "Pan"), ("PgUp/PgDn", "Page")],
        Mode::Diff(_) => &[
            ("Esc", "Close"),
            ("n", "Next"),
            ("p", "Prev"),
            ("f", "Fold"),
            ("←→", "Pan"),
        ],
        _ => &[
            ("1", "Help"),
            ("2", "Diff"),
            ("3", "View"),
            ("4", "Edit"),
            ("5", "Copy"),
            ("6", "Move"),
            ("7", "MkDir"),
            ("8", "Delete"),
            ("9", "CmpDirs"),
            ("o", "Options"),
            ("q", "Quit"),
        ],
    };
    let spans: Vec<Span> = keys
        .iter()
        .flat_map(|(k, label)| {
            [
                Span::raw(format!(" {k} ")).fg(ACCENT).bold(),
                Span::raw(format!("{label} ")).fg(DIM),
            ]
        })
        .collect();
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_viewer(f: &mut Frame, area: Rect, v: &mut Viewer) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(Line::raw(format!(" {} ", v.title)).fg(TEXT).bold());
    let inner = block.inner(area);
    f.render_widget(block, area);
    v.page = inner.height as usize;
    v.scroll = v.scroll.min(v.lines.len().saturating_sub(v.page));
    let gutter = v.lines.len().to_string().len();
    let lines: Vec<Line> = v
        .lines
        .iter()
        .enumerate()
        .skip(v.scroll)
        .take(v.page)
        .map(|(i, l)| {
            Line::from(vec![
                Span::raw(format!("{:>gutter$} ", i + 1)).fg(DIM),
                Span::raw(l.chars().skip(v.hscroll).collect::<String>()).white(),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_diff(f: &mut Frame, area: Rect, d: &mut DiffView) {
    let r = &d.response;
    let summary = match (&r.result, &r.note) {
        (_, Some(note)) => note.clone(),
        (Some(res), None) => format!(
            "+{} −{} ~{}  ·  change {}/{}",
            res.stats.added,
            res.stats.removed,
            res.stats.changed,
            d.hunk.map_or(0, |h| h + 1),
            res.hunks.len()
        ),
        (None, None) => String::new(),
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(Line::raw(" Diff ").fg(TEXT).bold())
        .title(Line::raw(format!(" {summary} ")).fg(DIM).right_aligned());
    let inner = block.inner(area);
    f.render_widget(block, area);

    let [titles, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);
    let [lt, _, rt] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(1), Constraint::Fill(1)]).areas(titles);
    let half = lt.width as usize;
    f.render_widget(Paragraph::new(elide_start(&r.left_title, half)).bold().white(), lt);
    f.render_widget(Paragraph::new(elide_start(&r.right_title, half)).bold().white(), rt);

    let Some(result) = &r.result else { return };
    d.page = body.height as usize;
    d.scroll = d.scroll.min(d.items.len().saturating_sub(d.page));
    let gutter = result.rows.len().to_string().len().max(3);
    let current = d.hunk.and_then(|h| result.hunks.get(h)).copied();

    let [lb, sep, rb] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(1), Constraint::Fill(1)]).areas(body);
    let mut left = Vec::new();
    let mut right = Vec::new();
    for item in d.items.iter().skip(d.scroll).take(d.page) {
        match *item {
            Visible::Row(i) => {
                let row = &result.rows[i];
                let marker = current == Some(i);
                left.push(side_line(
                    row,
                    row.left.as_ref(),
                    true,
                    gutter,
                    d.hscroll,
                    marker,
                    lb.width as usize,
                ));
                right.push(side_line(
                    row,
                    row.right.as_ref(),
                    false,
                    gutter,
                    d.hscroll,
                    marker,
                    rb.width as usize,
                ));
            }
            Visible::Fold { n, .. } => {
                let fold = Line::raw(format!("⋯ {n} unchanged line{}", if n == 1 { "" } else { "s" }))
                    .fg(DIM)
                    .centered();
                left.push(fold.clone());
                right.push(fold);
            }
        }
    }
    f.render_widget(Paragraph::new(left), lb);
    f.render_widget(Paragraph::new(right), rb);
    let bar: Vec<Line> = (0..sep.height)
        .map(|_| Line::raw("│").fg(Color::Indexed(240)))
        .collect();
    f.render_widget(Paragraph::new(bar), sep);
}

fn side_line<'a>(
    row: &DiffRow,
    side: Option<&'a Side>,
    is_left: bool,
    gutter: usize,
    hscroll: usize,
    current: bool,
    width: usize,
) -> Line<'a> {
    let (bg, strong) = match (row.kind, is_left) {
        (RowKind::Equal, _) => (None, None),
        (RowKind::Delete, _) | (RowKind::Replace, true) => (Some(DEL_BG), Some(DEL_STRONG)),
        (RowKind::Insert, _) | (RowKind::Replace, false) => (Some(INS_BG), Some(INS_STRONG)),
    };
    let marker = if current { "▶" } else { " " };
    let Some(side) = side else {
        // The other side has lines here: hatch the gap so rows stay aligned.
        let hatch = "╱".repeat(width.saturating_sub(gutter + 2));
        return Line::from(vec![
            Span::raw(format!("{marker}{:gutter$} ", "")),
            Span::raw(hatch).fg(HATCH),
        ]);
    };
    let mut spans = vec![Span::raw(format!("{marker}{:>gutter$} ", side.line_no)).fg(DIM)];
    let mut skip = hscroll;
    for seg in &side.segments {
        let text = sanitize(&seg.text);
        let n = text.chars().count();
        if skip >= n {
            skip -= n;
            continue;
        }
        let text: String = text.chars().skip(skip).collect();
        skip = 0;
        let style = match (seg.changed && row.kind == RowKind::Replace, strong) {
            (true, Some(s)) => Style::new().bg(s).fg(Color::White).add_modifier(Modifier::BOLD),
            _ => Style::new().fg(Color::Gray),
        };
        spans.push(Span::styled(text, style));
    }
    let line = Line::from(spans);
    match bg {
        Some(bg) => line.style(Style::new().bg(bg)),
        None => line,
    }
}

fn draw_options(f: &mut Frame, area: Rect, o: &Options, settings: &Settings) {
    let check = |on: bool| if on { "[x]" } else { "[ ]" };
    let mut lines = vec![Line::raw(" Columns").fg(ACCENT).bold()];
    let mut row = |i: usize, text: String| {
        let line = Line::raw(format!("  {text}"));
        lines.push(if i == o.cursor {
            line.black().on_cyan()
        } else {
            line.white()
        });
    };
    for (i, c) in Column::ALL.iter().enumerate() {
        row(i, format!("{} {}", check(settings.shows(*c)), c.label()));
    }
    row(HIDDEN_ROW, format!("{} Show hidden files", check(settings.show_hidden)));
    let editor = match &o.editing {
        Some(text) => format!("{text}█"),
        None if settings.editor.is_empty() => "($VISUAL / $EDITOR)".into(),
        None => settings.editor.clone(),
    };
    row(EDITOR_ROW, format!("Editor (F4): {editor}"));
    lines.push(Line::raw(""));
    lines.push(
        Line::raw(if o.editing.is_some() {
            " Enter keep · Esc discard the edit"
        } else {
            " ↑↓ move · Space toggle/edit · Enter save · Esc cancel · columns that don't fit are hidden"
        })
        .fg(DIM),
    );
    let area = popup(area, 60, lines.len() as u16 + 2);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(ACCENT))
                .title(Line::raw(" Options ").fg(TEXT).bold()),
        ),
        area,
    );
}

fn draw_help(f: &mut Frame, area: Rect) {
    const HELP: &[(&str, &str)] = &[
        ("↑↓ PgUp PgDn Home End", "move cursor"),
        ("Enter / →", "open directory, view file"),
        ("Backspace / ←", "parent directory"),
        ("Tab", "switch panel"),
        ("Space / Insert", "select / deselect"),
        ("+  -  *", "select by mask, deselect by mask, invert"),
        ("Esc", "clear selection and filter"),
        ("/  Ctrl+F", "quick filter"),
        (".", "show / hide hidden files"),
        ("o", "options: columns, hidden files, editor"),
        ("g  Ctrl+L", "go to path"),
        ("Ctrl+S  Ctrl+F3..F6", "cycle sort / sort by name, ext, date, size"),
        ("Ctrl+R  Ctrl+U", "refresh / swap panels"),
        ("F2", "diff: cursor files in both panels, or 2 selected"),
        ("F3  F4", "view / edit (editor set in options, else $EDITOR)"),
        ("F5  F6", "copy / move or rename"),
        ("F7  F8", "new directory / delete to Trash (Shift: permanent)"),
        ("F9", "compare directories (toggle)"),
        ("1..9", "same as F1..F9 (Mac: no fn needed)"),
        ("q  F10  Ctrl+C", "quit"),
    ];
    let lines: Vec<Line> = HELP
        .iter()
        .map(|(k, v)| {
            Line::from(vec![
                Span::raw(format!(" {k:<22}")).fg(ACCENT).bold(),
                Span::raw(*v).white(),
            ])
        })
        .collect();
    let area = popup(area, 70, lines.len() as u16 + 2);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(ACCENT))
                .title(Line::raw(" Keys ").fg(TEXT).bold()),
        ),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::sync::Arc;

    fn screen(app: &mut App) -> String {
        let mut term = Terminal::new(TestBackend::new(100, 20)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer();
        (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>() + "\n")
            .collect()
    }

    #[test]
    fn renders_panels_diff_and_help() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("dir")).unwrap();
        std::fs::write(tmp.path().join("a.txt"), "hello\nworld\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "hello\nthere\n").unwrap();
        let loc = rc_core::Location::local(tmp.path().to_string_lossy());
        let mut app = App::new(Arc::new(rc_vfs::Vfs::new()), loc.clone(), loc);

        let s = screen(&mut app);
        assert!(s.contains("dir/") && s.contains("a.txt") && s.contains("—"), "{s}");
        assert!(s.contains("Copy"), "function key bar");

        app.panels[0].focus_name("a.txt");
        app.panels[1].focus_name("b.txt");
        app.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
        let s = screen(&mut app);
        assert!(
            s.contains("world") && s.contains("there") && s.contains("change 0/1"),
            "{s}"
        );

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE));
        assert!(screen(&mut app).contains("quick filter"));
    }

    #[test]
    fn extreme_terminal_sizes_do_not_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let loc = rc_core::Location::local(tmp.path().to_string_lossy());
        let mut app = App::new(Arc::new(rc_vfs::Vfs::new()), loc.clone(), loc);
        for (w, h) in [(1000, 10), (10, 3), (1, 1)] {
            for code in [KeyCode::F(7), KeyCode::Esc, KeyCode::F(1), KeyCode::Esc] {
                let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
                term.draw(|f| draw(f, &mut app)).unwrap();
                app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
            }
        }
    }

    #[test]
    fn long_prompt_input_shows_its_end() {
        let tmp = tempfile::tempdir().unwrap();
        let loc = rc_core::Location::local(tmp.path().to_string_lossy());
        let mut app = App::new(Arc::new(rc_vfs::Vfs::new()), loc.clone(), loc);
        app.handle_key(KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE));
        for c in format!("{}END", "x".repeat(150)).chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert!(screen(&mut app).contains("END█"));
    }

    #[test]
    fn cursor_stays_visible_when_scrolling() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..100 {
            std::fs::write(tmp.path().join(format!("f{i:03}")), "").unwrap();
        }
        let loc = rc_core::Location::local(tmp.path().to_string_lossy());
        let mut app = App::new(Arc::new(rc_vfs::Vfs::new()), loc.clone(), loc);
        app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert!(screen(&mut app).contains("f099"));
        app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        let s = screen(&mut app);
        assert!(s.contains(" .. ") && !s.contains("f099"));
    }

    #[test]
    fn renders_configured_columns_and_options() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "x").unwrap();
        let loc = rc_core::Location::local(tmp.path().to_string_lossy());
        let mut app = App::new(Arc::new(rc_vfs::Vfs::new()), loc.clone(), loc);
        app.settings.toggle_column(Column::Permissions);
        app.settings.toggle_column(Column::Modified);
        let s = screen(&mut app);
        assert!(
            s.contains("Perms") && s.contains("-rw-") && !s.contains("Modified"),
            "{s}"
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
        let s = screen(&mut app);
        assert!(s.contains("[x] Permissions") && s.contains("[ ] Modified"), "{s}");
    }

    #[test]
    fn elides_long_paths_from_the_start() {
        assert_eq!(elide_start("/a/b/c/d", 5), "…/c/d");
        assert_eq!(elide_start("/a", 5), "/a");
    }
}
