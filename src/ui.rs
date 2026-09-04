//! Rendering with ratatui.

use crate::app::{
    App, ContainerMode, ContainerView, GridView, InfoView, ListState, SparseView, TextView, View,
};
use crate::format::{format_bytes, format_scalar, format_scalar_full, DisplayOpts};
use crate::model::Kind;
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, Wrap},
    Frame,
};
use unicode_width::UnicodeWidthStr;

pub struct Theme {
    pub accent: Color,
    pub dim: Color,
    pub highlight_bg: Color,
    pub header: Color,
    pub error: Color,
    pub class: Color,
    pub number: Color,
    pub text: Color,
}

pub const THEME: Theme = Theme {
    accent: Color::Cyan,
    dim: Color::DarkGray,
    highlight_bg: Color::Rgb(40, 60, 90),
    header: Color::Yellow,
    error: Color::Red,
    class: Color::Magenta,
    number: Color::Reset,
    text: Color::Green,
};

/// Rows available in the main area; used by the key handler for paging.
pub fn main_area_rows(size: Rect) -> usize {
    // title(1) + breadcrumb(1) + borders(2) + header row(1) + status(1)
    size.height.saturating_sub(6) as usize
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(area);

    draw_title(f, app, chunks[0]);
    draw_breadcrumb(f, app, chunks[1]);
    draw_main(f, app, chunks[2]);
    draw_status(f, app, chunks[3]);

    if app.show_help {
        draw_help(f, area);
    }
}

fn draw_title(f: &mut Frame, app: &App, area: Rect) {
    let nvars = app.file.variables().len();
    let left = Line::from(vec![
        Span::styled(
            " matreader ",
            Style::default().fg(Color::Black).bg(THEME.accent).bold(),
        ),
        Span::raw(" "),
        Span::styled(app.path.clone(), Style::default().bold()),
        Span::styled(
            format!(
                "  {}  ·  {} variable{}",
                app.file.format(),
                nvars,
                if nvars == 1 { "" } else { "s" }
            ),
            Style::default().fg(THEME.dim),
        ),
    ]);
    f.render_widget(Paragraph::new(left), area);

    // Non-default display options, right-aligned.
    let mut flags = Vec::new();
    if app.display.datetime {
        flags.push("dates".to_string());
    }
    if app.display.precision != DisplayOpts::DEFAULT_PRECISION {
        flags.push(format!("{} digits", app.display.precision));
    }
    if !flags.is_empty() {
        let right = Line::from(Span::styled(
            format!(" {} ", flags.join(" · ")),
            Style::default().fg(Color::Black).bg(THEME.header),
        ));
        f.render_widget(Paragraph::new(right).alignment(Alignment::Right), area);
    }
}

fn draw_breadcrumb(f: &mut Frame, app: &App, area: Rect) {
    let crumb = app.breadcrumb();
    let line = if crumb.is_empty() {
        Line::from(vec![Span::styled(" / ", Style::default().fg(THEME.dim))])
    } else {
        Line::from(vec![
            Span::styled(" ", Style::default()),
            Span::styled(crumb, Style::default().fg(THEME.accent).bold()),
        ])
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let (text, style) = if let Some(msg) = app.loading_message() {
        (msg, Style::default().fg(THEME.header))
    } else if let Some((msg, is_error, _)) = &app.status {
        (
            msg.clone(),
            if *is_error {
                Style::default().fg(THEME.error)
            } else {
                Style::default().fg(THEME.text)
            },
        )
    } else {
        let hints = match app.current() {
            View::Variables { list } if list.filtering => {
                "type to filter  Enter: keep  Esc: clear".to_string()
            }
            View::Container(c) if c.list.filtering => {
                "type to filter  Enter: keep  Esc: clear".to_string()
            }
            View::Variables { .. } => {
                "j/k: move  Enter: open  /: filter  ?: help  q: quit".to_string()
            }
            View::Container(_) => {
                "j/k: move  Enter: open  Esc: back  /: filter  ?: help  q: quit".to_string()
            }
            View::Grid(g) => {
                let mut s =
                    "hjkl: move  H/L: 5 cols  g/G: top/bottom  0/$: first/last col  Esc: back"
                        .to_string();
                if g.shape.pages() > 1 {
                    s.push_str("  [/]: page");
                }
                s
            }
            View::Text(_) => "j/k: scroll  h/l: horizontal  Esc: back  ?: help".to_string(),
            View::Sparse(_) => "j/k: move  Esc: back  ?: help".to_string(),
            View::Info(i) => {
                if i.has_inner {
                    "Enter: raw metadata  Esc: back".to_string()
                } else {
                    "Esc: back".to_string()
                }
            }
        };
        (hints, Style::default().fg(THEME.dim))
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(format!(" {}", text), style))),
        area,
    );
}

fn draw_main(f: &mut Frame, app: &mut App, area: Rect) {
    let file = app.file.clone();
    let opts = app.display;
    match app.current_mut() {
        View::Variables { list } => {
            let vars: Vec<_> = file.variables().to_vec();
            draw_variables(f, list, &vars, area);
        }
        View::Container(c) => draw_container(f, c, area, &opts),
        View::Grid(g) => draw_grid(f, g, area, &opts),
        View::Text(t) => draw_text(f, t, area),
        View::Sparse(s) => draw_sparse(f, s, area, &opts),
        View::Info(i) => draw_info(f, i, area),
    }
}

fn split_with_side(area: Rect) -> (Rect, Option<Rect>) {
    if area.width >= 100 {
        let chunks = Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)])
            .split(area);
        (chunks[0], Some(chunks[1]))
    } else {
        (area, None)
    }
}

fn block(title: String) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(THEME.dim))
        .title(Span::styled(
            format!(" {} ", title),
            Style::default().fg(THEME.header).bold(),
        ))
}

fn filter_title(list: &ListState) -> String {
    if list.filtering {
        format!("filter: {}▏", list.filter)
    } else if list.filtered.is_some() {
        format!("filter: {}", list.filter)
    } else {
        String::new()
    }
}

fn draw_variables(f: &mut Frame, list: &mut ListState, vars: &[crate::model::VarInfo], area: Rect) {
    let (main, side) = split_with_side(area);
    let total = vars.len();
    let height = main.height.saturating_sub(3) as usize; // borders + header
    list.ensure_visible(height);
    let visible = list.visible_count(total);

    let header = Row::new(vec!["Name", "Class", "Size", "Bytes", ""]).style(
        Style::default()
            .fg(THEME.header)
            .add_modifier(Modifier::BOLD),
    );
    let mut rows = Vec::new();
    for pos in list.offset..(list.offset + height).min(visible) {
        let i = list.item_at(pos);
        let v = &vars[i];
        let mut flags = Vec::new();
        if v.is_sparse {
            flags.push("sparse");
        }
        if v.is_complex {
            flags.push("complex");
        }
        if v.is_global {
            flags.push("global");
        }
        if v.compressed {
            flags.push("zip");
        }
        let style = if pos == list.selected {
            Style::default()
                .bg(THEME.highlight_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        rows.push(
            Row::new(vec![
                Cell::from(v.name.clone()),
                Cell::from(Span::styled(
                    v.class.name(),
                    Style::default().fg(THEME.class),
                )),
                Cell::from(v.shape.to_string()),
                Cell::from(Span::styled(
                    if v.storage_bytes > 0 {
                        format_bytes(v.storage_bytes)
                    } else {
                        String::new()
                    },
                    Style::default().fg(THEME.dim),
                )),
                Cell::from(Span::styled(
                    flags.join(" "),
                    Style::default().fg(THEME.dim),
                )),
            ])
            .style(style),
        );
    }
    let mut title = format!(
        "Variables ({}{})",
        visible,
        if visible != total {
            format!(" of {}", total)
        } else {
            String::new()
        }
    );
    let ft = filter_title(list);
    if !ft.is_empty() {
        title = format!("{} — {}", title, ft);
    }
    let table = Table::new(
        rows,
        [
            Constraint::Fill(3),
            Constraint::Length(16),
            Constraint::Fill(2),
            Constraint::Length(9),
            Constraint::Length(12),
        ],
    )
    .header(header)
    .block(block(title));
    f.render_widget(table, main);
    if visible == 0 {
        let msg = if total == 0 {
            "No variables in this file."
        } else {
            "No matches."
        };
        let inner = Rect {
            x: main.x + 2,
            y: main.y + 2,
            width: main.width.saturating_sub(4),
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Span::styled(msg, Style::default().fg(THEME.dim))),
            inner,
        );
    }

    if let Some(side) = side {
        let mut lines: Vec<Line> = Vec::new();
        if let Some(i) = list.selected_item(total) {
            let v = &vars[i];
            lines.push(Line::from(Span::styled(
                v.name.clone(),
                Style::default().bold(),
            )));
            lines.push(Line::from(""));
            let mut kv = vec![
                ("class", v.class.name()),
                ("size", v.shape.to_string()),
                ("elements", v.shape.numel().to_string()),
            ];
            if v.storage_bytes > 0 {
                kv.push((
                    if v.compressed {
                        "compressed bytes"
                    } else {
                        "bytes in file"
                    },
                    format_bytes(v.storage_bytes),
                ));
            }
            if v.is_sparse {
                kv.push(("sparse", "yes".into()));
            }
            if v.is_complex {
                kv.push(("complex", "yes".into()));
            }
            if v.is_global {
                kv.push(("global", "yes".into()));
            }
            for (k, val) in kv {
                lines.push(Line::from(vec![
                    Span::styled(format!("{:>17}  ", k), Style::default().fg(THEME.dim)),
                    Span::raw(val),
                ]));
            }
            lines.push(Line::from(""));
            let hint = match v.class {
                crate::model::Class::Opaque(_) => {
                    "Object contents are not decodable; Enter shows its metadata.".to_string()
                }
                _ if v.compressed && v.storage_bytes > 50 * 1024 * 1024 => {
                    "Large compressed variable: opening inflates it in the background.".to_string()
                }
                _ => "Press Enter to open.".to_string(),
            };
            lines.push(Line::from(Span::styled(
                hint,
                Style::default().fg(THEME.dim),
            )));
        }
        let p = Paragraph::new(lines)
            .block(block("Details".into()))
            .wrap(Wrap { trim: false });
        f.render_widget(p, side);
    }
}

fn draw_container(f: &mut Frame, c: &mut ContainerView, area: Rect, opts: &DisplayOpts) {
    let (main, side) = split_with_side(area);
    let total = c.len();
    let height = main.height.saturating_sub(3) as usize;
    c.list.ensure_visible(height);
    let visible = c.list.visible_count(total);
    let range = c.list.offset..(c.list.offset + height).min(visible);
    let selected_item = c.list.selected_item(total);
    App::ensure_summaries(
        c,
        range.clone(),
        if side.is_some() { selected_item } else { None },
        opts,
    );

    let label_header = match c.mode {
        ContainerMode::StructFields { .. } | ContainerMode::FunctionFields => "Field",
        ContainerMode::StructElements => "Element",
        ContainerMode::CellElements => "Cell",
    };
    let header = Row::new(vec![label_header, "Type", "Value"]).style(
        Style::default()
            .fg(THEME.header)
            .add_modifier(Modifier::BOLD),
    );
    let mut rows = Vec::new();
    for pos in range {
        let i = c.list.item_at(pos);
        let label = c.item_label(i);
        let (summary, inline, kind, err) = match c.summaries.get(&i) {
            Some(s) => (s.summary.clone(), s.inline.clone(), s.kind, s.error.clone()),
            None => (String::new(), String::new(), Kind::Unknown, None),
        };
        let style = if pos == c.list.selected {
            Style::default()
                .bg(THEME.highlight_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let value_cell = if let Some(e) = err {
            Cell::from(Span::styled(e, Style::default().fg(THEME.error)))
        } else {
            Cell::from(Span::styled(inline, value_style(kind)))
        };
        rows.push(
            Row::new(vec![
                Cell::from(label),
                Cell::from(Span::styled(summary, Style::default().fg(THEME.class))),
                value_cell,
            ])
            .style(style),
        );
    }
    let mut title = c.title();
    if visible != total {
        title = format!("{} ({} shown)", title, visible);
    }
    let ft = filter_title(&c.list);
    if !ft.is_empty() {
        title = format!("{} — {}", title, ft);
    }
    let table = Table::new(
        rows,
        [
            Constraint::Fill(2),
            Constraint::Fill(2),
            Constraint::Fill(3),
        ],
    )
    .header(header)
    .block(block(title));
    f.render_widget(table, main);
    if visible == 0 {
        let msg = if total == 0 {
            match c.mode {
                ContainerMode::CellElements => "Empty cell array.",
                ContainerMode::StructElements => "Empty struct array.",
                _ => "No fields.",
            }
        } else {
            "No matches."
        };
        let inner = Rect {
            x: main.x + 2,
            y: main.y + 2,
            width: main.width.saturating_sub(4),
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Span::styled(msg, Style::default().fg(THEME.dim))),
            inner,
        );
    }

    if let Some(side) = side {
        let mut lines: Vec<Line> = Vec::new();
        if let Some(i) = selected_item {
            if let Some(s) = c.summaries.get(&i) {
                lines.push(Line::from(Span::styled(
                    c.item_label(i),
                    Style::default().bold(),
                )));
                for (n, l) in s.preview.iter().enumerate() {
                    let style = if n == 0 {
                        Style::default().fg(THEME.class)
                    } else {
                        value_style(s.kind)
                    };
                    lines.push(Line::from(Span::styled(l.clone(), style)));
                }
                if let Some(e) = &s.error {
                    lines.push(Line::from(Span::styled(
                        e.clone(),
                        Style::default().fg(THEME.error),
                    )));
                }
            }
        }
        // Show the container's own storage info at the bottom.
        let info = c.node.info();
        if !info.is_empty() {
            lines.push(Line::from(""));
            for (k, v) in info {
                lines.push(Line::from(vec![
                    Span::styled(format!("{}: ", k), Style::default().fg(THEME.dim)),
                    Span::styled(v, Style::default().fg(THEME.dim)),
                ]));
            }
        }
        let p = Paragraph::new(lines)
            .block(block("Preview".into()))
            .wrap(Wrap { trim: false });
        f.render_widget(p, side);
    }
}

fn value_style(kind: Kind) -> Style {
    match kind {
        Kind::Char => Style::default().fg(THEME.text),
        Kind::Numeric | Kind::Sparse => Style::default().fg(THEME.number),
        _ => Style::default().fg(THEME.dim),
    }
}

#[allow(clippy::needless_range_loop)]
fn draw_grid(f: &mut Frame, g: &mut GridView, area: Rect, opts: &DisplayOpts) {
    let class = g.node.class();
    let shape = g.shape.clone();
    let mut title = crate::model::summarize(&*g.node).to_string();
    if shape.ndims() > 2 {
        let page: Vec<String> = g.page.iter().map(|p| (p + 1).to_string()).collect();
        title = format!(
            "{}  page (:,:,{})  [{}/{}]",
            title,
            page.join(","),
            g.page_index() + 1,
            shape.pages()
        );
    }
    let b = block(title);
    let inner = b.inner(area);
    f.render_widget(b, area);
    if inner.height < 3 || inner.width < 8 {
        return;
    }

    if shape.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                format!("Empty array ({})", shape),
                Style::default().fg(THEME.dim),
            )),
            inner,
        );
        return;
    }

    // Layout: header row (col indices), detail line at the bottom, rows in between.
    let detail_h = 2u16;
    let grid_h = inner.height.saturating_sub(1 + detail_h) as usize;
    let rows_visible = grid_h.max(1);
    let nrows = shape.rows();
    let ncols = shape.cols();

    // Keep cursor visible vertically.
    if g.row < g.row_off {
        g.row_off = g.row;
    } else if g.row >= g.row_off + rows_visible {
        g.row_off = g.row + 1 - rows_visible;
    }
    let row_end = (g.row_off + rows_visible).min(nrows);

    // Row header width.
    let row_hdr_w = (nrows.to_string().len() + 1).max(4);
    let avail_w = inner.width as usize - row_hdr_w - 1;

    // Determine how many columns fit, reading values for a generous window
    // first and then narrowing to what fits.
    if g.col < g.col_off {
        g.col_off = g.col;
    }
    let max_cols_guess = (avail_w / 4).max(1).min(ncols - g.col_off);
    let mut col_end = g.col_off + max_cols_guess;
    // Ensure cursor column within the window; shift window right if needed.
    loop {
        let vals = g.values(g.row_off..row_end, g.col_off..col_end).to_vec();
        let win_rows = row_end - g.row_off;
        let win_cols = col_end - g.col_off;
        let strs: Vec<String> = vals
            .iter()
            .map(|v| format_scalar(v, &class, opts))
            .collect();
        let mut widths = Vec::with_capacity(win_cols);
        for c in 0..win_cols {
            let mut w = (g.col_off + c + 1).to_string().len();
            for r in 0..win_rows {
                if let Some(s) = strs.get(r + c * win_rows) {
                    w = w.max(s.width());
                }
            }
            widths.push(w.min(24));
        }
        // How many columns actually fit?
        let mut used = 0usize;
        let mut fit = 0usize;
        for w in &widths {
            if used + w + 1 > avail_w {
                break;
            }
            used += w + 1;
            fit += 1;
        }
        let fit = fit.max(1).min(win_cols);
        if g.col >= g.col_off + fit && g.col_off + fit < ncols {
            // Scroll right so the cursor column is visible.
            g.col_off += 1;
            col_end = (g.col_off + max_cols_guess).min(ncols);
            if g.col_off > g.col {
                break;
            }
            continue;
        }
        // Render.
        let mut lines: Vec<Line> = Vec::with_capacity(rows_visible + 1);
        let mut hdr = vec![Span::styled(
            format!("{:>w$} ", "", w = row_hdr_w),
            Style::default(),
        )];
        for c in 0..fit {
            let col_idx = g.col_off + c;
            let style = if col_idx == g.col {
                Style::default().fg(THEME.header).bold()
            } else {
                Style::default().fg(THEME.dim)
            };
            hdr.push(Span::styled(
                format!("{:>w$} ", col_idx + 1, w = widths[c]),
                style,
            ));
        }
        lines.push(Line::from(hdr));
        for r in 0..win_rows {
            let row_idx = g.row_off + r;
            let row_style = if row_idx == g.row {
                Style::default().fg(THEME.header).bold()
            } else {
                Style::default().fg(THEME.dim)
            };
            let mut spans = vec![Span::styled(
                format!("{:>w$} ", row_idx + 1, w = row_hdr_w),
                row_style,
            )];
            for c in 0..fit {
                let col_idx = g.col_off + c;
                let s = strs.get(r + c * win_rows).cloned().unwrap_or_default();
                let s = if s.width() > widths[c] {
                    let mut t: String = s.chars().take(widths[c].saturating_sub(1)).collect();
                    t.push('…');
                    t
                } else {
                    s
                };
                let style = if row_idx == g.row && col_idx == g.col {
                    Style::default()
                        .bg(THEME.highlight_bg)
                        .add_modifier(Modifier::BOLD)
                } else if row_idx == g.row || col_idx == g.col {
                    Style::default().bg(Color::Rgb(30, 30, 40))
                } else {
                    Style::default()
                };
                spans.push(Span::styled(format!("{:>w$}", s, w = widths[c]), style));
                spans.push(Span::raw(" "));
            }
            lines.push(Line::from(spans));
        }
        let grid_area = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: inner.height.saturating_sub(detail_h),
        };
        f.render_widget(Paragraph::new(lines), grid_area);

        // Detail line.
        let detail_area = Rect {
            x: inner.x,
            y: inner.y + inner.height - detail_h,
            width: inner.width,
            height: detail_h,
        };
        let value = g
            .current_value()
            .map(|v| format_scalar_full(&v, &class, opts))
            .unwrap_or_default();
        let mut detail = vec![Line::from(vec![
            Span::styled(
                g.current_subscripts(),
                Style::default().fg(THEME.accent).bold(),
            ),
            Span::raw(" = "),
            Span::styled(value, Style::default().bold()),
        ])];
        let mut info_parts: Vec<String> = g
            .node
            .info()
            .into_iter()
            .map(|(k, v)| format!("{}: {}", k, v))
            .collect();
        if let Some(e) = &g.error {
            info_parts.insert(0, format!("error: {}", e));
        }
        detail.push(Line::from(Span::styled(
            info_parts.join("  ·  "),
            Style::default().fg(THEME.dim),
        )));
        f.render_widget(Paragraph::new(detail), detail_area);
        break;
    }
}

fn draw_text(f: &mut Frame, t: &mut TextView, area: Rect) {
    let shape = t.shape.clone();
    let mut title = crate::model::summarize(&*t.node).to_string();
    if shape.ndims() > 2 {
        let page: Vec<String> = t.page.iter().map(|p| (p + 1).to_string()).collect();
        title = format!("{}  page (:,:,{})", title, page.join(","));
    }
    let b = block(title);
    let inner = b.inner(area);
    f.render_widget(b, area);
    if shape.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                "'' (empty char)",
                Style::default().fg(THEME.dim),
            )),
            inner,
        );
        return;
    }
    let nrows = shape.rows();
    let hdr_w = if nrows > 1 {
        nrows.to_string().len() + 1
    } else {
        0
    };
    let height = inner.height as usize;
    let mut lines = Vec::new();
    for r in t.row_off..(t.row_off + height).min(nrows) {
        let s = t.row(r);
        let shown: String = s.chars().skip(t.col_off).collect();
        let mut spans = Vec::new();
        if hdr_w > 0 {
            spans.push(Span::styled(
                format!("{:>w$} ", r + 1, w = hdr_w - 1),
                Style::default().fg(THEME.dim),
            ));
        }
        spans.push(Span::styled(shown, Style::default().fg(THEME.text)));
        lines.push(Line::from(spans));
    }
    if t.col_off > 0 {
        lines.insert(
            0,
            Line::from(Span::styled(
                format!("← column offset {}", t.col_off),
                Style::default().fg(THEME.dim),
            )),
        );
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_sparse(f: &mut Frame, s: &mut SparseView, area: Rect, opts: &DisplayOpts) {
    let class = s.node.class();
    let (main, side) = split_with_side(area);
    let height = main.height.saturating_sub(3) as usize;
    s.list.ensure_visible(height);
    let total = s.nnz;
    let range = s.list.offset..(s.list.offset + height).min(total);
    s.ensure(range.clone());
    let header = Row::new(vec!["#", "Row", "Col", "Value"]).style(
        Style::default()
            .fg(THEME.header)
            .add_modifier(Modifier::BOLD),
    );
    let mut rows = Vec::new();
    for k in range {
        let style = if k == s.list.selected {
            Style::default()
                .bg(THEME.highlight_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let (r, c, v) = match s.cache.get(&k) {
            Some(e) => (
                (e.row + 1).to_string(),
                (e.col + 1).to_string(),
                format_scalar(&e.value, &class, opts),
            ),
            None => (String::new(), String::new(), String::new()),
        };
        rows.push(
            Row::new(vec![
                Cell::from((k + 1).to_string()),
                Cell::from(r),
                Cell::from(c),
                Cell::from(v),
            ])
            .style(style),
        );
    }
    let title = format!("{}  ·  nnz = {}", crate::model::summarize(&*s.node), total);
    let table = Table::new(
        rows,
        [
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Fill(1),
        ],
    )
    .header(header)
    .block(block(title));
    f.render_widget(table, main);
    if total == 0 {
        let inner = Rect {
            x: main.x + 2,
            y: main.y + 2,
            width: main.width.saturating_sub(4),
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Span::styled(
                "All-zero sparse matrix.",
                Style::default().fg(THEME.dim),
            )),
            inner,
        );
    }
    if let Some(side) = side {
        let mut lines: Vec<Line> = vec![
            Line::from(vec![
                Span::styled("size  ", Style::default().fg(THEME.dim)),
                Span::raw(s.shape.to_string()),
            ]),
            Line::from(vec![
                Span::styled("nnz   ", Style::default().fg(THEME.dim)),
                Span::raw(total.to_string()),
            ]),
            Line::from(vec![
                Span::styled("density  ", Style::default().fg(THEME.dim)),
                Span::raw(if s.shape.numel() > 0 {
                    format!("{:.3}%", 100.0 * total as f64 / s.shape.numel() as f64)
                } else {
                    "n/a".into()
                }),
            ]),
        ];
        if let Some(e) = &s.error {
            lines.push(Line::from(Span::styled(
                e.clone(),
                Style::default().fg(THEME.error),
            )));
        }
        for (k, v) in s.node.info() {
            lines.push(Line::from(vec![
                Span::styled(format!("{}: ", k), Style::default().fg(THEME.dim)),
                Span::styled(v, Style::default().fg(THEME.dim)),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Entries are listed in compressed-sparse-column order.",
            Style::default().fg(THEME.dim),
        )));
        f.render_widget(
            Paragraph::new(lines)
                .block(block("Details".into()))
                .wrap(Wrap { trim: false }),
            side,
        );
    }
}

fn draw_info(f: &mut Frame, i: &mut InfoView, area: Rect) {
    let b = block(crate::model::summarize(&*i.node));
    let inner = b.inner(area);
    f.render_widget(b, area);
    let max_scroll = i.lines.len().saturating_sub(inner.height as usize);
    if i.scroll > max_scroll {
        i.scroll = max_scroll;
    }
    let lines: Vec<Line> = i
        .lines
        .iter()
        .skip(i.scroll)
        .map(|l| Line::from(l.clone()))
        .collect();
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let text = [
        ("Navigation", ""),
        ("j / k, ↑ / ↓", "move selection or cursor"),
        ("Enter, l, →", "open the selected variable / field / cell"),
        ("Esc, Backspace, h, ←", "go back up one level"),
        ("g / G, Home / End", "jump to first / last"),
        ("PgUp / PgDn, Ctrl-u / Ctrl-d", "page"),
        ("/", "filter the current list (Esc clears)"),
        ("", ""),
        ("Grid view (numeric arrays)", ""),
        ("h / l", "move one column (h at column 1 goes back)"),
        ("H / L", "move five columns"),
        ("0 / $", "first / last column"),
        ("[ / ]  (p / n)", "previous / next page of an N-D array"),
        ("", ""),
        ("Text view (char arrays)", ""),
        ("h / l", "scroll horizontally"),
        ("", ""),
        ("Display", ""),
        ("D", "toggle showing numbers as datenums (dates)"),
        ("+ / -", "more / fewer significant digits"),
        ("", ""),
        ("General", ""),
        ("?", "toggle this help"),
        ("q, Ctrl-c", "quit"),
    ];
    let width = 70u16.min(area.width.saturating_sub(4));
    let height = (text.len() as u16 + 4).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: (area.width.saturating_sub(width)) / 2,
        y: (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, popup);
    let mut lines = Vec::new();
    for (k, v) in text {
        if v.is_empty() {
            lines.push(Line::from(Span::styled(
                k,
                Style::default().fg(THEME.header).bold(),
            )));
        } else {
            lines.push(Line::from(vec![
                Span::styled(format!("  {:<30}", k), Style::default().fg(THEME.accent)),
                Span::raw(v),
            ]));
        }
    }
    let p = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(THEME.accent))
                .title(Span::styled(
                    " Help (Esc to close) ",
                    Style::default().fg(THEME.header).bold(),
                ))
                .title_alignment(Alignment::Center),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(p, popup);
}
