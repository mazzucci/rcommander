//! Rendering. Colors use the 256-color palette because macOS Terminal.app has no truecolor.

use super::app::{App, LEFT, Mode};
use super::views::{DiffView, Viewer, sanitize};
use chrono::{Local, Offset, TimeZone};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Paragraph, Row as TableRow, Table};
use rc_core::diff::{DiffRow, RowKind, Side, Visible};
use rc_core::dircmp::CmpMark;
use rc_core::format::{format_time, human_size};
use rc_core::{Panel, Row};

const CURSOR_ACTIVE: Color = Color::Cyan;
const CURSOR_INACTIVE: Color = Color::Indexed(238);
const DEL_BG: Color = Color::Indexed(52);
const DEL_STRONG: Color = Color::Indexed(124);
const INS_BG: Color = Color::Indexed(22);
const INS_STRONG: Color = Color::Indexed(28);
const CHG_BG: Color = Color::Indexed(17);
const DIM: Color = Color::Indexed(244);

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
            let input = format!("{}█", p.input);
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .title(p.title.as_str())
                .cyan();
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
                .title("Confirm")
                .red();
            let text = vec![
                Line::raw(c.text.as_str()).white(),
                Line::raw(""),
                Line::raw("[Enter/y] yes    [Esc/n] no").fg(DIM),
            ];
            f.render_widget(Clear, area);
            f.render_widget(Paragraph::new(text).alignment(Alignment::Center).block(block), area);
        }
        Mode::Help => draw_help(f, main),
        _ => {}
    }
}

fn popup(area: Rect, width_pct: u16, height: u16) -> Rect {
    let w = (area.width * width_pct / 100).max(30).min(area.width);
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

fn local_time(secs: i64) -> String {
    let offset = Local
        .timestamp_opt(secs, 0)
        .single()
        .map_or(0, |d| d.offset().fix().local_minus_utc());
    format_time(secs, offset.into())
}

fn mark_color(m: CmpMark) -> Color {
    match m {
        CmpMark::Orphan => Color::Green,
        CmpMark::Newer => Color::LightMagenta,
        CmpMark::Older => Color::Blue,
        CmpMark::Different => Color::Red,
        CmpMark::Same => DIM,
    }
}

fn panel_status(p: &Panel) -> String {
    let (n, size) = p.selection_summary();
    let files = p.entries().iter().filter(|e| !e.is_dir).count();
    let mut s = format!(" {n} of {files} selected, {} ", human_size(size));
    s += &format!("· {}{} ", p.sort.label(), if p.sort_desc { "▼" } else { "▲" });
    if !p.filter.is_empty() {
        s += &format!("· filter: {} ", p.filter);
    }
    if p.show_hidden {
        s += "· hidden shown ";
    }
    s
}

fn draw_panel(f: &mut Frame, area: Rect, app: &mut App, side: usize) {
    let is_active = side == app.active;
    let filtering = is_active && matches!(app.mode, Mode::Filter);
    let p = &app.panels[side];
    let border = if is_active { Color::Cyan } else { Color::Indexed(240) };
    let title = elide_start(&format!(" {} ", p.location), area.width.saturating_sub(4) as usize);
    let status = if filtering {
        format!(" filter: {}█ ", p.filter)
    } else {
        panel_status(p)
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border))
        .title(Line::raw(title).bold().fg(if is_active { Color::White } else { DIM }))
        .title_bottom(Line::raw(status).fg(if filtering { Color::Yellow } else { DIM }));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Header row + list; keep the cursor inside the visible window.
    let height = inner.height.saturating_sub(1) as usize;
    app.page = height.saturating_sub(1).max(1);
    let scroll = &mut app.scroll[side];
    if p.cursor < *scroll {
        *scroll = p.cursor;
    } else if height > 0 && p.cursor >= *scroll + height {
        *scroll = p.cursor + 1 - height;
    }
    let scroll = *scroll;

    let wide = inner.width >= 48;
    let cursor_bg = if is_active { CURSOR_ACTIVE } else { CURSOR_INACTIVE };
    let rows = (scroll..p.len().min(scroll + height)).map(|r| {
        let (mark, name, size, date, mut style) = match p.rows()[r] {
            Row::Parent => (
                Cell::from(" "),
                "[..]".to_string(),
                "<UP>".to_string(),
                String::new(),
                Style::new().fg(Color::White).bold(),
            ),
            Row::Entry(_) => {
                let e = p.row_entry(r).expect("entry row");
                let mark = p.marks.get(&e.name).copied();
                let mut style = if e.is_dir {
                    Style::new().fg(Color::White).bold()
                } else {
                    Style::new().fg(Color::Gray)
                };
                if e.hidden {
                    style = style.fg(DIM);
                }
                if let Some(m) = mark.filter(|m| *m != CmpMark::Same) {
                    style = style.fg(mark_color(m));
                }
                if p.is_selected(e) {
                    style = style.fg(Color::Yellow).bold();
                }
                let name = if e.is_dir {
                    format!("[{}]", e.name)
                } else {
                    e.name.clone()
                };
                let name = if e.is_link { format!("{name} →") } else { name };
                let size = if e.is_dir {
                    "<DIR>".into()
                } else {
                    e.size.map(human_size).unwrap_or_default()
                };
                let mark = mark.map_or(Cell::from(" "), |m| {
                    Cell::from(m.symbol().to_string()).style(Style::new().fg(mark_color(m)))
                });
                (mark, name, size, e.modified.map(local_time).unwrap_or_default(), style)
            }
        };
        if r == p.cursor {
            style = style
                .bg(cursor_bg)
                .fg(if is_active { Color::Black } else { Color::White });
        }
        let mut cells = vec![
            mark,
            Cell::from(name),
            Cell::from(Line::raw(size).alignment(Alignment::Right)),
        ];
        if wide {
            cells.push(Cell::from(date));
        }
        TableRow::new(cells).style(style)
    });

    let mut widths = vec![Constraint::Length(1), Constraint::Fill(1), Constraint::Length(7)];
    let mut header = vec!["", "Name", "   Size"];
    if wide {
        widths.push(Constraint::Length(16));
        header.push("Date");
    }
    let table = Table::new(rows, widths)
        .header(TableRow::new(header).style(Style::new().fg(Color::Cyan).bold()))
        .column_spacing(1);
    f.render_widget(table, inner);
}

fn draw_message(f: &mut Frame, area: Rect, app: &App) {
    let line = if let Some(job) = app.job_status() {
        Line::raw(format!(" ⏳ {job}")).yellow()
    } else if let Some((msg, is_err)) = &app.message {
        let l = Line::raw(format!(" {msg}"));
        if *is_err { l.red() } else { l.green() }
    } else {
        Line::raw(" F1/? help").fg(DIM)
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
            ("10", "Quit"),
        ],
    };
    let spans: Vec<Span> = keys
        .iter()
        .flat_map(|(k, label)| {
            [
                Span::raw(format!(" {k}")).white().bold(),
                Span::raw(format!("{label:<7}")).black().on_cyan(),
            ]
        })
        .collect();
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_viewer(f: &mut Frame, area: Rect, v: &mut Viewer) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(v.title.as_str())
        .cyan();
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
        .title(Line::raw(" Diff ").bold())
        .title(Line::raw(format!(" {summary} ")).right_aligned())
        .cyan();
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
                left.push(side_line(row, row.left.as_ref(), true, gutter, d.hscroll, marker));
                right.push(side_line(row, row.right.as_ref(), false, gutter, d.hscroll, marker));
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
) -> Line<'a> {
    let (bg, strong) = match (row.kind, is_left) {
        (RowKind::Equal, _) => (None, None),
        (RowKind::Delete, _) | (RowKind::Replace, true) => (Some(DEL_BG), Some(DEL_STRONG)),
        (RowKind::Insert, _) | (RowKind::Replace, false) => (Some(INS_BG), Some(INS_STRONG)),
    };
    let marker = if current { "▶" } else { " " };
    let Some(side) = side else {
        // The other side has lines here; show an empty band so rows stay aligned.
        let filler = Style::new().bg(if row.kind == RowKind::Equal {
            Color::Reset
        } else {
            CHG_BG
        });
        return Line::from(Span::raw(format!("{marker}{:gutter$} ", ""))).style(filler);
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
        ("g  Ctrl+L", "go to path"),
        ("Ctrl+S  Ctrl+F3..F6", "cycle sort / sort by name, ext, date, size"),
        ("Ctrl+R  Ctrl+U", "refresh / swap panels"),
        ("F2", "diff: cursor files in both panels, or 2 selected"),
        ("F3  F4", "view / edit with $EDITOR"),
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
                Span::raw(format!(" {k:<22}")).cyan().bold(),
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
                .title(" Keys ")
                .cyan(),
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
        assert!(s.contains("[dir]") && s.contains("a.txt") && s.contains("<DIR>"), "{s}");
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
    fn elides_long_paths_from_the_start() {
        assert_eq!(elide_start("/a/b/c/d", 5), "…/c/d");
        assert_eq!(elide_start("/a", 5), "/a");
    }
}
