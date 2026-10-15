//! Rendering. Colors use the 256-color palette because macOS Terminal.app has no truecolor.

use super::app::{App, EDITOR_ROW, HIDDEN_ROW, LEFT, Mode, Options, RIGHT};
use super::find::{Field, Find};
use super::views::{DiffView, Viewer, sanitize};
use chrono::{Local, Offset, TimeZone};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Paragraph, Row as TableRow, Table};
use rc_core::Settings;
use rc_core::api::Preview;
use rc_core::diff::{DiffRow, RowKind, Side, Visible};
use rc_core::dircmp::CmpMark;
use rc_core::format::human_size;
use rc_core::{Column, Location, Panel, Row, SortKey};

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
const FLASH_BG: Color = Color::Indexed(23);

pub fn draw(f: &mut Frame, app: &mut App) {
    let [main, message, keys] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1), Constraint::Length(1)]).areas(f.area());

    match &mut app.mode {
        Mode::Viewer(v) | Mode::FindView(_, v) => draw_viewer(f, main, v),
        Mode::Diff(d) => draw_diff(f, main, d),
        _ => {
            let [l, r] = Layout::horizontal([Constraint::Percentage(50); 2]).areas(main);
            draw_side(f, l, app, LEFT);
            draw_side(f, r, app, RIGHT);
        }
    }

    draw_message(f, message, app);
    draw_keys(f, keys, &app.mode);
    if let Mode::Find(find) = &mut app.mode {
        draw_find(f, main, find);
    }

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
        Mode::Hotlist(cursor) => draw_hotlist(f, main, *cursor, &app.settings.bookmarks),
        Mode::Conflict(c) => {
            let area = popup(main, 64, 5);
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(RULE))
                .title(Line::raw(" Already exists ").fg(SELECTED).bold());
            let folders = c.existing.iter().filter(|p| std::path::Path::new(p).is_dir()).count();
            let name = |p: &String| p.rsplit('/').next().unwrap_or(p).to_string();
            let what = match c.existing.as_slice() {
                [one] if folders == 1 => format!("Folder {} already exists; its contents will be merged.", name(one)),
                [one] => format!("{} already exists.", name(one)),
                many => format!("{} items already exist at the destination.", many.len()),
            };
            let key = |k: &'static str, label: &'static str| [Span::raw(k).fg(ACCENT).bold(), Span::raw(label).fg(DIM)];
            let choices: Vec<Span> = [
                key("o", " replace existing   "),
                key("u", " replace older   "),
                key("s", " keep existing   "),
                key("Esc", " cancel"),
            ]
            .into_iter()
            .flatten()
            .collect();
            let text = vec![Line::raw(what).fg(TEXT), Line::raw(""), Line::from(choices)];
            f.render_widget(Clear, area);
            f.render_widget(Paragraph::new(text).alignment(Alignment::Center).block(block), area);
        }
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

/// A panel, or with Quick View on, the preview in the panel that is not active.
fn draw_side(f: &mut Frame, area: Rect, app: &mut App, side: usize) {
    if app.quick_view && side != app.active {
        draw_preview(f, area, app);
    } else {
        draw_panel(f, area, app, side);
    }
}

fn draw_preview(f: &mut Frame, area: Rect, app: &App) {
    let name = app
        .preview
        .as_ref()
        .and_then(|((loc, ..), _)| loc.file_name().map(str::to_string));
    let title = elide_start(
        &format!(
            " Quick view{} ",
            name.map(|n| format!(": {}", sanitize(&n))).unwrap_or_default()
        ),
        area.width.saturating_sub(4) as usize,
    );
    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(RULE))
        .title(Line::raw(title).bold().fg(META));
    let centered = |text: String, color: Color| (vec![Line::raw(text).fg(color)], true);
    let (lines, center) = match app.preview.as_ref().map(|(_, p)| p) {
        None => centered("Nothing to preview · Ctrl+Q close".into(), DIM),
        Some(Err(e)) => centered(e.clone(), ERR),
        Some(Ok(Preview::Special { what, .. })) => centered(format!("A {what}: nothing to show"), DIM),
        Some(Ok(Preview::Folder(sum))) => {
            let plural = |n: usize, w: &str| format!("{n} {w}{}", if n == 1 { "" } else { "s" });
            let mut lines = vec![
                Line::raw("Folder").fg(DIR).bold(),
                Line::raw(""),
                Line::raw(plural(sum.folders, "folder")).fg(TEXT),
                Line::raw(format!("{} · {}", plural(sum.files, "file"), human_size(sum.size))).fg(TEXT),
            ];
            if sum.hidden > 0 {
                lines.push(Line::raw(format!("{} hidden", sum.hidden)).fg(DIM));
            }
            lines.push(Line::raw(""));
            lines.push(
                Line::raw(if sum.partial {
                    "Very large folder: counting stopped early"
                } else {
                    "Subfolders not included"
                })
                .fg(DIM),
            );
            (lines, true)
        }
        Some(Ok(Preview::File(c))) => match &c.text {
            None => centered(format!("Binary file · {} · F3 to view", human_size(c.size)), DIM),
            Some(text) => {
                let note = if c.truncated {
                    format!(" {} · beginning shown · F3 to view ", human_size(c.size))
                } else {
                    format!(" {} ", human_size(c.size))
                };
                block = block.title_bottom(Line::raw(note).fg(DIM));
                let height = area.height.saturating_sub(2) as usize;
                let gutter = text.lines().take(height).count().to_string().len();
                let lines = text
                    .lines()
                    .take(height)
                    .enumerate()
                    .map(|(i, l)| {
                        Line::from(vec![
                            Span::raw(format!("{:>gutter$} ", i + 1)).fg(DIM),
                            Span::raw(sanitize(l)).fg(TEXT),
                        ])
                    })
                    .collect();
                (lines, false)
            }
        },
    };
    let inner = block.inner(area);
    f.render_widget(block, area);
    if center {
        let h = (lines.len() as u16).min(inner.height);
        let [_, middle, _] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(h), Constraint::Fill(1)]).areas(inner);
        f.render_widget(Paragraph::new(lines).alignment(Alignment::Center), middle);
    } else {
        f.render_widget(Paragraph::new(lines), inner);
    }
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
                        match (c, e.git) {
                            (Column::Git, Some(g)) => v.style(Style::new().fg(Color::Indexed(g.color().1)).bold()),
                            _ if selected => v,
                            _ => v.style(Style::new().fg(META)),
                        }
                    })
                    .collect();
                (mark, name, values, style)
            }
        };
        // Rows that just changed on disk light up briefly.
        if p.row_entry(r).is_some_and(|e| app.flash[side].contains_key(&e.name)) {
            style = style.bg(FLASH_BG);
        }
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
        Line::raw(format!(" {job}")).fg(SELECTED)
    } else if let Some((msg, is_err)) = &app.message {
        Line::raw(format!(" {msg}")).fg(if *is_err { ERR } else { OK })
    } else {
        Line::raw(" ? help · o options").fg(DIM)
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_keys(f: &mut Frame, area: Rect, mode: &Mode) {
    let keys: &[(&str, &str)] = match mode {
        Mode::Viewer(_) | Mode::FindView(..) => {
            &[("Esc", "Close"), ("↑↓", "Scroll"), ("←→", "Pan"), ("PgUp/PgDn", "Page")]
        }
        Mode::Find(f) if f.search.is_some() => &[("Esc", "Stop")],
        Mode::Find(f) if f.result.is_some() => &[("Enter", "Go to"), ("F3", "View"), ("Esc", "New search")],
        Mode::Find(_) => &[
            ("Enter", "Search"),
            ("Tab", "Next field"),
            ("Space", "Toggle"),
            ("Esc", "Close"),
        ],
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
            let line = Line::from(vec![
                Span::raw(format!("{:>gutter$} ", i + 1)).fg(DIM),
                Span::raw(l.chars().skip(v.hscroll).collect::<String>()).white(),
            ]);
            if v.mark == Some(i) {
                line.bg(CURSOR_INACTIVE)
            } else {
                line
            }
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

fn draw_hotlist(f: &mut Frame, area: Rect, cursor: usize, marks: &[String]) {
    let mut lines: Vec<Line> = marks
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let n = if i < 9 { format!("{} ", i + 1) } else { "  ".into() };
            let line = Line::from(vec![
                Span::raw(format!(" {n}")).fg(ACCENT).bold(),
                Span::raw(m.as_str()),
            ]);
            if i == cursor {
                line.fg(Color::White).bg(CURSOR_ACTIVE)
            } else {
                line.fg(TEXT)
            }
        })
        .collect();
    if marks.is_empty() {
        lines.push(Line::raw(" No bookmarks yet.").fg(DIM));
    }
    lines.push(Line::raw(""));
    lines.push(Line::raw(" Enter/1-9 go · a add current folder · d remove · Esc close").fg(DIM));
    let area = popup(area, 60, lines.len() as u16 + 2);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(ACCENT))
                .title(Line::raw(" Hotlist ").fg(TEXT).bold()),
        ),
        area,
    );
}

fn draw_find(f: &mut Frame, area: Rect, find: &mut Find) {
    let area = popup(area, 80, area.height.saturating_sub(2).max(10));
    let Location::Local { path: root } = &find.root;
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(
            Line::raw(format!(
                " Find files in {} ",
                elide_start(root, area.width.saturating_sub(20) as usize)
            ))
            .fg(TEXT)
            .bold(),
        );
    let inner = block.inner(area);
    f.render_widget(Clear, area);
    f.render_widget(block, area);
    let [fields, status, list, hint] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    let editing = find.search.is_none() && find.result.is_none();
    let field = |label: &'static str, value: &str, which: Field| {
        let active = editing && find.field == which;
        Line::from(vec![
            Span::raw(format!(" {label:<9}"))
                .fg(if active { ACCENT } else { META })
                .bold(),
            Span::raw(format!("{}{}", sanitize(value), if active { "█" } else { "" })).fg(TEXT),
        ])
    };
    let hidden_active = editing && find.field == Field::Hidden;
    let hidden = Line::from(vec![
        Span::raw(format!(" {:<9}", "")),
        Span::raw(format!(
            "[{}] Search hidden files and folders",
            if find.hidden { "x" } else { " " }
        ))
        .fg(if hidden_active { ACCENT } else { META }),
    ]);
    let hidden = if hidden_active { hidden.bold() } else { hidden };
    f.render_widget(
        Paragraph::new(vec![
            field("Name", &find.name, Field::Name),
            field("Contains", &find.contains, Field::Contains),
            hidden,
        ]),
        fields,
    );

    let status_line = if let Some(search) = &find.search {
        let (dir, n) = search
            .progress
            .as_ref()
            .map_or((String::new(), 0), |p| (find.relative(&p.path).to_string(), p.done));
        let dir = elide_start(&sanitize(&dir), status.width.saturating_sub(30) as usize);
        let at = if dir.is_empty() || dir == find.root.to_string() {
            String::new()
        } else {
            format!(" {dir}")
        };
        Line::raw(format!(" Searching{at} · {n} found")).fg(SELECTED)
    } else if let Some(e) = &find.error {
        Line::raw(format!(" {e}")).fg(ERR)
    } else if let Some(r) = &find.result {
        let mut s = format!(" {} found", r.hits.len());
        if r.truncated {
            s += " (limit reached)";
        }
        if r.cancelled {
            s += " · stopped";
        }
        if r.unreadable > 0 {
            s += &format!(" · {} unreadable", r.unreadable);
        }
        Line::raw(s).fg(if r.hits.is_empty() { DIM } else { OK })
    } else {
        Line::raw("")
    };
    f.render_widget(Paragraph::new(status_line), status);

    if editing {
        let help = vec![
            Line::raw(" Name: masks like *.rs;*.toml — a plain word matches inside names; empty: all").fg(DIM),
            Line::raw(" Contains: text in files (case-sensitive only if it has capitals); empty: names only").fg(DIM),
        ];
        f.render_widget(Paragraph::new(help), list);
    } else {
        find.page = list.height as usize;
        if find.cursor < find.scroll {
            find.scroll = find.cursor;
        } else if find.cursor >= find.scroll + find.page {
            find.scroll = find.cursor + 1 - find.page;
        }
        let lines: Vec<Line> = find
            .hits()
            .iter()
            .enumerate()
            .skip(find.scroll)
            .take(find.page)
            .map(|(i, hit)| {
                let rel = sanitize(find.relative(&hit.path));
                let mut spans = vec![if hit.is_dir {
                    Span::raw(format!(" {rel}/")).fg(DIR)
                } else {
                    Span::raw(format!(" {rel}")).fg(TEXT)
                }];
                if let (Some(n), Some(text)) = (hit.line, &hit.text) {
                    spans.push(Span::raw(format!(":{n}  ")).fg(DIM));
                    spans.push(Span::raw(sanitize(text)).fg(META));
                }
                let line = Line::from(spans);
                if i == find.cursor { line.bg(CURSOR_ACTIVE) } else { line }
            })
            .collect();
        f.render_widget(Paragraph::new(lines), list);
    }
    let keys = if find.search.is_some() {
        " Esc stop"
    } else if find.result.is_some() {
        " Enter go to · F3 view · Esc new search"
    } else {
        " Enter search · Tab next field · Space toggle · Esc close"
    };
    f.render_widget(Paragraph::new(Line::raw(keys).fg(DIM)), hint);
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
        ("z", "jump with zoxide"),
        ("Alt+←/→  [  ]", "back / forward"),
        ("Ctrl+D", "hotlist (bookmarks)"),
        ("c", "Claude's changes: files an agent edited (F2 diff, F8 forget)"),
        ("Ctrl+S  Ctrl+F3..F6", "cycle sort / sort by name, ext, date, size"),
        ("Ctrl+R  Ctrl+U", "refresh / swap panels"),
        ("F2", "diff: cursor files in both panels, or 2 selected"),
        ("Shift+F2", "diff the file against git HEAD"),
        ("F3  F4", "view / edit (editor set in options, else $EDITOR)"),
        ("F5  F6", "copy / move or rename"),
        ("F7  F8", "new directory / delete to Trash (Shift: permanent)"),
        ("F9", "compare directories (toggle)"),
        (
            "Ctrl+Q  Shift+Q",
            "quick view: the other panel previews the file or folder",
        ),
        ("Alt+F7  Shift+F", "find files (name mask, text inside)"),
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
    fn renders_the_find_dialog() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("notes.md"), "a\nfind me\n").unwrap();
        let loc = rc_core::Location::local(tmp.path().to_string_lossy());
        let mut app = App::new(Arc::new(rc_vfs::Vfs::new()), loc.clone(), loc);
        app.handle_key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT));
        let s = screen(&mut app);
        assert!(
            s.contains("Find files in") && s.contains("Contains") && s.contains("Enter search"),
            "{s}"
        );
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        for c in "find".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let start = std::time::Instant::now();
        while !screen(&mut app).contains("1 found") {
            app.tick();
            assert!(start.elapsed().as_secs() < 10, "{}", screen(&mut app));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let s = screen(&mut app);
        assert!(s.contains("notes.md:2  find me") && s.contains("Go to"), "{s}");
    }

    #[test]
    fn quick_view_previews_the_entry_under_the_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("dir")).unwrap();
        std::fs::write(tmp.path().join("dir/x"), "").unwrap();
        std::fs::write(tmp.path().join("notes.txt"), "first line\nsecond line\n").unwrap();
        std::fs::write(tmp.path().join("blob.bin"), b"\0\x01\x02").unwrap();
        let loc = rc_core::Location::local(tmp.path().to_string_lossy());
        let mut app = App::new(Arc::new(rc_vfs::Vfs::new()), loc.clone(), loc);
        let ctrl_q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
        app.panels[0].focus_name("notes.txt");
        app.handle_key(ctrl_q);
        let s = screen(&mut app);
        assert!(
            s.contains("Quick view: notes.txt") && s.contains("2 second line"),
            "{s}"
        );
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert!(screen(&mut app).contains("Quick view: blob.bin"), "follows the cursor");
        app.panels[0].focus_name("dir");
        app.sync_preview();
        let s = screen(&mut app);
        assert!(s.contains("Quick view: dir") && s.contains("1 file"), "{s}");
        app.panels[0].focus_name("blob.bin");
        app.sync_preview();
        assert!(screen(&mut app).contains("Binary file"));
        // Tab: the preview moves to the other side, following the new active panel.
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(screen(&mut app).contains("Quick view"));
        app.handle_key(ctrl_q);
        assert!(!screen(&mut app).contains("Quick view") && app.preview.is_none());

        // A named pipe is described, never opened for reading (that would hang).
        let fifo = tmp.path().join("pipe");
        let c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        app.panels[app.active].focus_name("pipe");
        app.handle_key(KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::SHIFT));
        assert!(screen(&mut app).contains("named pipe"));
    }

    #[test]
    fn extreme_terminal_sizes_do_not_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let loc = rc_core::Location::local(tmp.path().to_string_lossy());
        let mut app = App::new(Arc::new(rc_vfs::Vfs::new()), loc.clone(), loc);
        std::fs::write(tmp.path().join("hit.txt"), "a needle").unwrap();
        let draw_at = |app: &mut App, w: u16, h: u16| {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| draw(f, app)).unwrap();
        };
        for (w, h) in [(1000, 10), (40, 6), (10, 3), (1, 1)] {
            for code in [KeyCode::F(7), KeyCode::Esc, KeyCode::F(1), KeyCode::Esc] {
                draw_at(&mut app, w, h);
                app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
            }
            // Find files in every state: fields, searching, results, viewing a hit.
            app.handle_key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT));
            app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
            draw_at(&mut app, w, h);
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            draw_at(&mut app, w, h);
            let start = std::time::Instant::now();
            while matches!(&app.mode, Mode::Find(f) if f.search.is_some()) {
                app.tick();
                assert!(start.elapsed().as_secs() < 10);
            }
            draw_at(&mut app, w, h);
            app.handle_key(KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE));
            draw_at(&mut app, w, h);
            for _ in 0..3 {
                app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                draw_at(&mut app, w, h);
            }
            assert!(matches!(app.mode, Mode::Normal));
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
