//! Application state: the navigation stack of views, background loading, and
//! key handling.

use crate::format::{format_scalar, sanitize_text, DisplayOpts};
use crate::model::{
    decode_chars, summarize, Kind, MatFile, Node, NodeRef, Scalar, Shape, SparseEntry, Window,
};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::Instant;

/// A one-line and multi-line description of a child node, cached per item.
#[derive(Debug, Clone)]
pub struct ChildSummary {
    /// e.g. `3x4 double`
    pub summary: String,
    /// e.g. `42`, `'hello'`, `[1 2 3]`
    pub inline: String,
    /// Multi-line preview for the side panel.
    pub preview: Vec<String>,
    pub kind: Kind,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerMode {
    /// Fields of struct element `elem`.
    StructFields { elem: usize },
    /// Elements of a struct array.
    StructElements,
    /// Elements of a cell array.
    CellElements,
    /// Fields of a function handle's underlying struct.
    FunctionFields,
}

/// Shared list navigation state.
#[derive(Debug, Clone, Default)]
pub struct ListState {
    pub selected: usize,
    pub offset: usize,
    pub filter: String,
    pub filtering: bool,
    /// Indices (into the full item list) that pass the filter; `None` = all.
    pub filtered: Option<Vec<usize>>,
}

impl ListState {
    pub fn visible_count(&self, total: usize) -> usize {
        match &self.filtered {
            Some(f) => f.len(),
            None => total,
        }
    }

    /// Map a position in the (filtered) list to an item index.
    pub fn item_at(&self, pos: usize) -> usize {
        match &self.filtered {
            Some(f) => f.get(pos).copied().unwrap_or(0),
            None => pos,
        }
    }

    pub fn selected_item(&self, total: usize) -> Option<usize> {
        if self.visible_count(total) == 0 {
            None
        } else {
            Some(self.item_at(self.selected))
        }
    }

    pub fn move_by(&mut self, delta: isize, total: usize) {
        let n = self.visible_count(total);
        if n == 0 {
            return;
        }
        let cur = self.selected as isize;
        self.selected = (cur + delta).clamp(0, n as isize - 1) as usize;
    }

    pub fn ensure_visible(&mut self, height: usize) {
        if height == 0 {
            return;
        }
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + height {
            self.offset = self.selected + 1 - height;
        }
    }
}

pub struct ContainerView {
    pub node: NodeRef,
    pub mode: ContainerMode,
    pub fields: Vec<String>,
    pub list: ListState,
    pub summaries: HashMap<usize, ChildSummary>,
}

impl ContainerView {
    pub fn len(&self) -> usize {
        match self.mode {
            ContainerMode::StructFields { .. } | ContainerMode::FunctionFields => self.fields.len(),
            ContainerMode::StructElements | ContainerMode::CellElements => {
                self.node.shape().numel()
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn item_label(&self, i: usize) -> String {
        match self.mode {
            ContainerMode::StructFields { .. } | ContainerMode::FunctionFields => {
                self.fields.get(i).cloned().unwrap_or_default()
            }
            ContainerMode::StructElements => self.node.shape().format_subscripts(i),
            ContainerMode::CellElements => {
                let s = self.node.shape().format_subscripts(i);
                format!("{{{}}}", &s[1..s.len() - 1])
            }
        }
    }

    /// Breadcrumb segment for item `i`.
    pub fn crumb(&self, i: usize) -> String {
        match self.mode {
            ContainerMode::StructFields { .. } | ContainerMode::FunctionFields => {
                format!(".{}", self.fields.get(i).cloned().unwrap_or_default())
            }
            ContainerMode::StructElements => self.node.shape().format_subscripts(i),
            ContainerMode::CellElements => {
                let s = self.node.shape().format_subscripts(i);
                format!("{{{}}}", &s[1..s.len() - 1])
            }
        }
    }

    pub fn child(&self, i: usize) -> Result<Box<dyn Node>> {
        match self.mode {
            ContainerMode::StructFields { elem } => self.node.field(elem, i),
            ContainerMode::FunctionFields => self.node.field(0, i),
            ContainerMode::CellElements => self.node.cell(i),
            ContainerMode::StructElements => anyhow::bail!("struct elements are opened in place"),
        }
    }

    pub fn title(&self) -> String {
        let shape = self.node.shape();
        match self.mode {
            ContainerMode::StructFields { elem } => {
                if shape.numel() > 1 {
                    format!(
                        "struct element {} of {} — {} fields",
                        shape.format_subscripts(elem),
                        shape,
                        self.fields.len()
                    )
                } else {
                    format!("{} — {} fields", summarize(&*self.node), self.fields.len())
                }
            }
            ContainerMode::FunctionFields => {
                format!("function handle — {} fields", self.fields.len())
            }
            ContainerMode::StructElements => {
                format!("{} — {} elements", summarize(&*self.node), shape.numel())
            }
            ContainerMode::CellElements => {
                format!("{} — {} elements", summarize(&*self.node), shape.numel())
            }
        }
    }

    pub fn filter_matches(&self, i: usize, needle: &str) -> bool {
        let label = self.item_label(i).to_lowercase();
        if label.contains(needle) {
            return true;
        }
        if let Some(s) = self.summaries.get(&i) {
            return s.inline.to_lowercase().contains(needle)
                || s.summary.to_lowercase().contains(needle);
        }
        false
    }
}

pub struct GridView {
    pub node: NodeRef,
    pub shape: Shape,
    /// Subscripts for dimensions >= 2 (the current 2-D page).
    pub page: Vec<usize>,
    pub row: usize,
    pub col: usize,
    pub row_off: usize,
    pub col_off: usize,
    /// Cached window and values.
    pub cache: Option<(Window, Vec<Scalar>)>,
    pub error: Option<String>,
}

impl GridView {
    pub fn new(node: NodeRef) -> GridView {
        let shape = node.shape();
        let page = vec![0; shape.ndims().saturating_sub(2)];
        GridView {
            node,
            shape,
            page,
            row: 0,
            col: 0,
            row_off: 0,
            col_off: 0,
            cache: None,
            error: None,
        }
    }

    pub fn values(
        &mut self,
        rows: std::ops::Range<usize>,
        cols: std::ops::Range<usize>,
    ) -> &[Scalar] {
        let window = Window::page(rows, cols, &self.page).clamp(&self.shape);
        let stale = match &self.cache {
            Some((w, _)) => *w != window,
            None => true,
        };
        if stale {
            match self.node.read_window(&window) {
                Ok(v) => {
                    self.error = None;
                    self.cache = Some((window, v));
                }
                Err(e) => {
                    self.error = Some(e.to_string());
                    self.cache = Some((window, Vec::new()));
                }
            }
        }
        &self.cache.as_ref().unwrap().1
    }

    pub fn cached_window(&self) -> Option<&Window> {
        self.cache.as_ref().map(|(w, _)| w)
    }

    pub fn current_value(&self) -> Option<Scalar> {
        let (w, v) = self.cache.as_ref()?;
        if w.len.len() < 2 || w.len[0] == 0 {
            return None;
        }
        let r = self.row.checked_sub(w.start[0])?;
        let c = self.col.checked_sub(w.start[1])?;
        if r >= w.len[0] || c >= w.len[1] {
            return None;
        }
        v.get(r + c * w.len[0]).cloned()
    }

    pub fn page_index(&self) -> usize {
        let mut idx = 0;
        let mut stride = 1;
        for (i, &p) in self.page.iter().enumerate() {
            idx += p * stride;
            stride *= self.shape.dims()[i + 2];
        }
        idx
    }

    pub fn set_page_index(&mut self, mut idx: usize) {
        for (i, p) in self.page.iter_mut().enumerate() {
            let d = self.shape.dims()[i + 2].max(1);
            *p = idx % d;
            idx /= d;
        }
        self.cache = None;
    }

    pub fn move_cursor(&mut self, dr: isize, dc: isize) {
        let rows = self.shape.rows();
        let cols = self.shape.cols();
        if rows > 0 {
            self.row = (self.row as isize + dr).clamp(0, rows as isize - 1) as usize;
        }
        if cols > 0 {
            self.col = (self.col as isize + dc).clamp(0, cols as isize - 1) as usize;
        }
    }

    /// Subscript string of the current cell, one-based, e.g. `(3,7)` or `(3,7,2)`.
    pub fn current_subscripts(&self) -> String {
        let mut parts = vec![(self.row + 1).to_string(), (self.col + 1).to_string()];
        for p in &self.page {
            parts.push((p + 1).to_string());
        }
        format!("({})", parts.join(","))
    }
}

pub struct TextView {
    pub node: NodeRef,
    pub shape: Shape,
    pub page: Vec<usize>,
    pub row_off: usize,
    pub col_off: usize,
    pub rows: HashMap<usize, String>,
}

impl TextView {
    pub fn new(node: NodeRef) -> TextView {
        let shape = node.shape();
        let page = vec![0; shape.ndims().saturating_sub(2)];
        TextView {
            node,
            shape,
            page,
            row_off: 0,
            col_off: 0,
            rows: HashMap::new(),
        }
    }

    pub fn row(&mut self, r: usize) -> String {
        if let Some(s) = self.rows.get(&r) {
            return s.clone();
        }
        let s = match self.node.text_row(r, &self.page) {
            Ok(t) => sanitize_text(&t),
            Err(e) => format!("<error: {}>", e),
        };
        if self.rows.len() > 4096 {
            self.rows.clear();
        }
        self.rows.insert(r, s.clone());
        s
    }
}

pub struct SparseView {
    pub node: NodeRef,
    pub shape: Shape,
    pub nnz: usize,
    pub list: ListState,
    pub cache: HashMap<usize, SparseEntry>,
    pub error: Option<String>,
}

impl SparseView {
    pub fn new(node: NodeRef) -> SparseView {
        let shape = node.shape();
        let nnz = node.nnz();
        SparseView {
            node,
            shape,
            nnz,
            list: ListState::default(),
            cache: HashMap::new(),
            error: None,
        }
    }

    pub fn ensure(&mut self, range: std::ops::Range<usize>) {
        let missing = range.clone().any(|k| !self.cache.contains_key(&k));
        if !missing {
            return;
        }
        if self.cache.len() > 10_000 {
            self.cache.clear();
        }
        match self.node.sparse_entries(range.clone()) {
            Ok(entries) => {
                for (i, e) in entries.into_iter().enumerate() {
                    self.cache.insert(range.start + i, e);
                }
                self.error = None;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }
}

pub struct InfoView {
    pub node: NodeRef,
    pub lines: Vec<String>,
    pub scroll: usize,
    pub has_inner: bool,
}

pub enum View {
    Variables { list: ListState },
    Container(ContainerView),
    Grid(GridView),
    Text(TextView),
    Sparse(SparseView),
    Info(InfoView),
}

pub struct Crumb {
    pub label: String,
    pub view: View,
}

struct JobResult {
    id: u64,
    result: Result<Box<dyn Node>>,
}

struct Pending {
    id: u64,
    cancel: Arc<AtomicBool>,
    label: String,
    crumb: String,
    started: Instant,
    /// Open the result as struct element fields rather than by kind.
    as_struct_elem: Option<usize>,
}

pub struct App {
    pub file: Arc<dyn MatFile>,
    pub path: String,
    pub views: Vec<Crumb>,
    pub show_help: bool,
    pub should_quit: bool,
    pub status: Option<(String, bool, Instant)>,
    pub spinner: usize,
    /// How numeric values are rendered (precision, datenum-as-date).
    pub display: DisplayOpts,
    pending: Option<Pending>,
    next_job: u64,
    tx: Sender<JobResult>,
    rx: Receiver<JobResult>,
}

const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

impl App {
    pub fn new(file: Box<dyn MatFile>, path: String) -> App {
        let (tx, rx) = mpsc::channel();
        App {
            file: Arc::from(file),
            path,
            views: vec![Crumb {
                label: String::new(),
                view: View::Variables {
                    list: ListState::default(),
                },
            }],
            show_help: false,
            should_quit: false,
            status: None,
            spinner: 0,
            display: DisplayOpts::default(),
            pending: None,
            next_job: 1,
            tx,
            rx,
        }
    }

    pub fn current(&self) -> &View {
        &self.views.last().unwrap().view
    }

    pub fn current_mut(&mut self) -> &mut View {
        &mut self.views.last_mut().unwrap().view
    }

    pub fn breadcrumb(&self) -> String {
        self.views
            .iter()
            .map(|c| c.label.as_str())
            .collect::<String>()
    }

    pub fn is_loading(&self) -> bool {
        self.pending.is_some()
    }

    pub fn loading_message(&self) -> Option<String> {
        self.pending.as_ref().map(|p| {
            let secs = p.started.elapsed().as_secs_f64();
            format!(
                "{} Loading {}{}  (Esc to cancel)",
                SPINNER[self.spinner % SPINNER.len()],
                p.label,
                if secs >= 1.0 {
                    format!(" — {:.0}s", secs)
                } else {
                    String::new()
                }
            )
        })
    }

    pub fn set_status(&mut self, msg: impl Into<String>, is_error: bool) {
        self.status = Some((msg.into(), is_error, Instant::now()));
    }

    /// Called on every tick: advance spinner, collect finished jobs, expire messages.
    pub fn tick(&mut self) {
        self.spinner = self.spinner.wrapping_add(1);
        if let Some((_, is_error, t)) = &self.status {
            let ttl = if *is_error { 8 } else { 3 };
            if t.elapsed().as_secs() >= ttl {
                self.status = None;
            }
        }
        while let Ok(res) = self.rx.try_recv() {
            let Some(p) = self.pending.as_ref() else {
                continue;
            };
            if p.id != res.id {
                continue;
            }
            let p = self.pending.take().unwrap();
            match res.result {
                Ok(node) => {
                    let node: NodeRef = Arc::from(node);
                    let view = match p.as_struct_elem {
                        Some(elem) => View::Container(ContainerView {
                            fields: node.field_names().unwrap_or_default(),
                            node,
                            mode: ContainerMode::StructFields { elem },
                            list: ListState::default(),
                            summaries: HashMap::new(),
                        }),
                        None => Self::view_for(node),
                    };
                    self.views.push(Crumb {
                        label: p.crumb,
                        view,
                    });
                }
                Err(e) => {
                    if !e.to_string().contains("cancelled") {
                        self.set_status(format!("Failed to open {}: {}", p.label, e), true);
                    }
                }
            }
        }
    }

    /// Choose a view for a loaded node.
    pub fn view_for(node: NodeRef) -> View {
        match node.kind() {
            Kind::Numeric => View::Grid(GridView::new(node)),
            Kind::Char => View::Text(TextView::new(node)),
            Kind::Cell => View::Container(ContainerView {
                fields: Vec::new(),
                node,
                mode: ContainerMode::CellElements,
                list: ListState::default(),
                summaries: HashMap::new(),
            }),
            Kind::Struct => {
                let fields = node.field_names().unwrap_or_default();
                let mode = if node.shape().numel() == 1 {
                    ContainerMode::StructFields { elem: 0 }
                } else {
                    ContainerMode::StructElements
                };
                View::Container(ContainerView {
                    node,
                    mode,
                    fields,
                    list: ListState::default(),
                    summaries: HashMap::new(),
                })
            }
            Kind::Function => View::Container(ContainerView {
                fields: node.field_names().unwrap_or_default(),
                node,
                mode: ContainerMode::FunctionFields,
                list: ListState::default(),
                summaries: HashMap::new(),
            }),
            Kind::Sparse => View::Sparse(SparseView::new(node)),
            Kind::Opaque | Kind::Unknown => {
                let mut lines = Vec::new();
                lines.push(summarize(&*node));
                if let Some(d) = node.description() {
                    lines.push(String::new());
                    lines.extend(wrap(&d, 70));
                }
                let info = node.info();
                if !info.is_empty() {
                    lines.push(String::new());
                    for (k, v) in info {
                        lines.push(format!("{}: {}", k, v));
                    }
                }
                let has_inner = matches!(node.inner(), Ok(Some(_)));
                if has_inner {
                    lines.push(String::new());
                    lines.push("Press Enter to inspect the raw object metadata.".into());
                }
                View::Info(InfoView {
                    node,
                    lines,
                    scroll: 0,
                    has_inner,
                })
            }
        }
    }

    fn start_job<F>(&mut self, label: String, crumb: String, as_struct_elem: Option<usize>, job: F)
    where
        F: FnOnce(&AtomicBool) -> Result<Box<dyn Node>> + Send + 'static,
    {
        if let Some(p) = self.pending.take() {
            p.cancel.store(true, Ordering::Relaxed);
        }
        let id = self.next_job;
        self.next_job += 1;
        let cancel = Arc::new(AtomicBool::new(false));
        let tx = self.tx.clone();
        let c2 = cancel.clone();
        std::thread::spawn(move || {
            let result = job(&c2).and_then(|node| {
                node.prepare()?;
                Ok(node)
            });
            let _ = tx.send(JobResult { id, result });
        });
        self.pending = Some(Pending {
            id,
            cancel,
            label,
            crumb,
            started: Instant::now(),
            as_struct_elem,
        });
    }

    fn cancel_job(&mut self) {
        if let Some(p) = self.pending.take() {
            p.cancel.store(true, Ordering::Relaxed);
            self.set_status(format!("Cancelled loading {}", p.label), false);
        }
    }

    /// Open the selected item of the current view.
    fn open_selected(&mut self) {
        let file = self.file.clone();
        let crumbs = self.breadcrumb();
        match self.current_mut() {
            View::Variables { list } => {
                let total = file.variables().len();
                let Some(idx) = list.selected_item(total) else {
                    return;
                };
                let info = file.variables()[idx].clone();
                let label = info.name.clone();
                self.start_job(label.clone(), label, None, move |cancel| {
                    file.load(idx, cancel)
                });
            }
            View::Container(c) => {
                let total = c.len();
                let Some(idx) = c.list.selected_item(total) else {
                    return;
                };
                let crumb = c.crumb(idx);
                let label = format!("{}{}", crumbs, crumb);
                match c.mode {
                    ContainerMode::StructElements => {
                        let node = c.node.clone();
                        self.start_job(label, crumb, Some(idx), move |_| {
                            // Same node, viewed as one element's fields.
                            Ok(Box::new(SharedNode(node)) as Box<dyn Node>)
                        });
                    }
                    _ => {
                        let node = c.node.clone();
                        let mode = c.mode;
                        self.start_job(label, crumb, None, move |_| match mode {
                            ContainerMode::StructFields { elem } => node.field(elem, idx),
                            ContainerMode::FunctionFields => node.field(0, idx),
                            ContainerMode::CellElements => node.cell(idx),
                            ContainerMode::StructElements => unreachable!(),
                        });
                    }
                }
            }
            View::Info(info) if info.has_inner => {
                let node = info.node.clone();
                let label = format!("{} metadata", crumbs);
                self.start_job(label, ".<metadata>".into(), None, move |_| {
                    node.inner()?
                        .ok_or_else(|| anyhow::anyhow!("no inner data"))
                });
            }
            _ => {}
        }
    }

    /// Drop cached renderings that depend on `self.display`.
    fn display_changed(&mut self) {
        for crumb in &mut self.views {
            if let View::Container(c) = &mut crumb.view {
                c.summaries.clear();
            }
        }
    }

    fn go_back(&mut self) -> bool {
        if self.views.len() > 1 {
            self.views.pop();
            true
        } else {
            false
        }
    }

    /// Handle a key press. `page` is the number of rows visible in the main area.
    pub fn handle_key(&mut self, key: KeyEvent, page: usize) {
        let page = page.max(1);
        if self.show_help {
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Enter => {
                    self.show_help = false
                }
                _ => {}
            }
            return;
        }

        // Filter typing mode.
        if let Some(list) = self.filtering_list() {
            match key.code {
                KeyCode::Esc => {
                    list.filtering = false;
                    list.filter.clear();
                    list.filtered = None;
                    list.selected = 0;
                    list.offset = 0;
                }
                KeyCode::Enter => {
                    list.filtering = false;
                    if list.filter.is_empty() {
                        list.filtered = None;
                    }
                }
                KeyCode::Backspace => {
                    list.filter.pop();
                    self.apply_filter();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    list.filter.push(c);
                    self.apply_filter();
                }
                KeyCode::Down => {
                    list.move_by(1, usize::MAX);
                }
                KeyCode::Up => {
                    list.move_by(-1, usize::MAX);
                }
                _ => {}
            }
            return;
        }

        if self.pending.is_some() {
            match key.code {
                KeyCode::Esc => self.cancel_job(),
                KeyCode::Char('q') => self.should_quit = true,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.should_quit = true
                }
                _ => {}
            }
            return;
        }

        // Global keys.
        match key.code {
            KeyCode::Char('q') => {
                self.should_quit = true;
                return;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
                return;
            }
            KeyCode::Char('?') => {
                self.show_help = true;
                return;
            }
            KeyCode::Char('D') => {
                self.display.datetime = !self.display.datetime;
                self.display_changed();
                let msg = if self.display.datetime {
                    "Showing numbers as datenums (dates)"
                } else {
                    "Showing numbers as numbers"
                };
                self.set_status(msg, false);
                return;
            }
            KeyCode::Char('+') | KeyCode::Char('=') | KeyCode::Char('-') => {
                let p = &mut self.display.precision;
                *p = if key.code == KeyCode::Char('-') {
                    p.saturating_sub(1).max(DisplayOpts::MIN_PRECISION)
                } else {
                    (*p + 1).min(DisplayOpts::MAX_PRECISION)
                };
                let msg = format!("Precision: {} significant digits", self.display.precision);
                self.display_changed();
                self.set_status(msg, false);
                return;
            }
            KeyCode::Esc | KeyCode::Backspace => {
                // Clear an active filter first, then go back.
                if let Some(list) = self.active_list() {
                    if list.filtered.is_some() {
                        list.filter.clear();
                        list.filtered = None;
                        list.selected = 0;
                        list.offset = 0;
                        return;
                    }
                }
                if !self.go_back() && key.code == KeyCode::Esc {
                    // At the top level, Esc does nothing.
                }
                return;
            }
            _ => {}
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match self.current_mut() {
            View::Variables { .. } | View::Container(_) | View::Sparse(_) => {
                let total = self.current_total();
                let is_sparse = matches!(self.current(), View::Sparse(_));
                let list = self.active_list().unwrap();
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => list.move_by(1, total),
                    KeyCode::Char('k') | KeyCode::Up => list.move_by(-1, total),
                    KeyCode::PageDown => list.move_by(page as isize, total),
                    KeyCode::PageUp => list.move_by(-(page as isize), total),
                    KeyCode::Char('d') if ctrl => list.move_by((page / 2) as isize, total),
                    KeyCode::Char('u') if ctrl => list.move_by(-((page / 2) as isize), total),
                    KeyCode::Char('g') | KeyCode::Home => list.move_by(isize::MIN / 2, total),
                    KeyCode::Char('G') | KeyCode::End => list.move_by(isize::MAX / 2, total),
                    KeyCode::Char('/') => {
                        if !is_sparse {
                            list.filtering = true;
                        }
                    }
                    KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => self.open_selected(),
                    KeyCode::Char('h') | KeyCode::Left => {
                        self.go_back();
                    }
                    _ => {}
                }
            }
            View::Grid(g) => {
                let half = (page / 2) as isize;
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => g.move_cursor(1, 0),
                    KeyCode::Char('k') | KeyCode::Up => g.move_cursor(-1, 0),
                    KeyCode::Char('h') | KeyCode::Left => {
                        if g.col == 0 {
                            self.go_back();
                        } else {
                            g.move_cursor(0, -1)
                        }
                    }
                    KeyCode::Char('l') | KeyCode::Right => g.move_cursor(0, 1),
                    KeyCode::Char('H') => g.move_cursor(0, -5),
                    KeyCode::Char('L') => g.move_cursor(0, 5),
                    KeyCode::PageDown => g.move_cursor(page as isize, 0),
                    KeyCode::PageUp => g.move_cursor(-(page as isize), 0),
                    KeyCode::Char('d') if ctrl => g.move_cursor(half, 0),
                    KeyCode::Char('u') if ctrl => g.move_cursor(-half, 0),
                    KeyCode::Char('g') | KeyCode::Home => g.move_cursor(isize::MIN / 2, 0),
                    KeyCode::Char('G') | KeyCode::End => g.move_cursor(isize::MAX / 2, 0),
                    KeyCode::Char('0') | KeyCode::Char('^') => g.move_cursor(0, isize::MIN / 2),
                    KeyCode::Char('$') => g.move_cursor(0, isize::MAX / 2),
                    KeyCode::Char(']') | KeyCode::Char('n') => {
                        let pages = g.shape.pages();
                        if pages > 1 {
                            let idx = (g.page_index() + 1) % pages;
                            g.set_page_index(idx);
                        }
                    }
                    KeyCode::Char('[') | KeyCode::Char('p') => {
                        let pages = g.shape.pages();
                        if pages > 1 {
                            let idx = (g.page_index() + pages - 1) % pages;
                            g.set_page_index(idx);
                        }
                    }
                    _ => {}
                }
            }
            View::Text(t) => {
                let rows = t.shape.rows();
                let max_off = rows.saturating_sub(1);
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => t.row_off = (t.row_off + 1).min(max_off),
                    KeyCode::Char('k') | KeyCode::Up => t.row_off = t.row_off.saturating_sub(1),
                    KeyCode::PageDown => t.row_off = (t.row_off + page).min(max_off),
                    KeyCode::PageUp => t.row_off = t.row_off.saturating_sub(page),
                    KeyCode::Char('d') if ctrl => t.row_off = (t.row_off + page / 2).min(max_off),
                    KeyCode::Char('u') if ctrl => t.row_off = t.row_off.saturating_sub(page / 2),
                    KeyCode::Char('g') | KeyCode::Home => t.row_off = 0,
                    KeyCode::Char('G') | KeyCode::End => t.row_off = max_off,
                    KeyCode::Char('h') | KeyCode::Left => {
                        if t.col_off == 0 {
                            self.go_back();
                        } else {
                            t.col_off = t.col_off.saturating_sub(8)
                        }
                    }
                    KeyCode::Char('l') | KeyCode::Right => t.col_off += 8,
                    KeyCode::Char('0') | KeyCode::Char('^') => t.col_off = 0,
                    KeyCode::Char(']') | KeyCode::Char('n') => {
                        let pages = t.shape.pages();
                        if pages > 1 {
                            let idx = (page_index(&t.page, &t.shape) + 1) % pages;
                            set_page_index(&mut t.page, &t.shape, idx);
                            t.rows.clear();
                        }
                    }
                    KeyCode::Char('[') | KeyCode::Char('p') => {
                        let pages = t.shape.pages();
                        if pages > 1 {
                            let idx = (page_index(&t.page, &t.shape) + pages - 1) % pages;
                            set_page_index(&mut t.page, &t.shape, idx);
                            t.rows.clear();
                        }
                    }
                    _ => {}
                }
            }
            View::Info(info) => match key.code {
                KeyCode::Char('j') | KeyCode::Down => info.scroll += 1,
                KeyCode::Char('k') | KeyCode::Up => info.scroll = info.scroll.saturating_sub(1),
                KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => self.open_selected(),
                KeyCode::Char('h') | KeyCode::Left => {
                    self.go_back();
                }
                _ => {}
            },
        }
    }

    fn current_total(&self) -> usize {
        match self.current() {
            View::Variables { .. } => self.file.variables().len(),
            View::Container(c) => c.len(),
            View::Sparse(s) => s.nnz,
            _ => 0,
        }
    }

    fn active_list(&mut self) -> Option<&mut ListState> {
        match self.current_mut() {
            View::Variables { list } => Some(list),
            View::Container(c) => Some(&mut c.list),
            View::Sparse(s) => Some(&mut s.list),
            _ => None,
        }
    }

    fn filtering_list(&mut self) -> Option<&mut ListState> {
        let l = self.active_list()?;
        if l.filtering {
            Some(l)
        } else {
            None
        }
    }

    fn apply_filter(&mut self) {
        let file = self.file.clone();
        match self.current_mut() {
            View::Variables { list } => {
                let needle = list.filter.to_lowercase();
                if needle.is_empty() {
                    list.filtered = None;
                } else {
                    list.filtered = Some(
                        file.variables()
                            .iter()
                            .enumerate()
                            .filter(|(_, v)| {
                                v.name.to_lowercase().contains(&needle)
                                    || v.class.name().to_lowercase().contains(&needle)
                            })
                            .map(|(i, _)| i)
                            .collect(),
                    );
                }
                list.selected = 0;
                list.offset = 0;
            }
            View::Container(c) => {
                let needle = c.list.filter.to_lowercase();
                if needle.is_empty() {
                    c.list.filtered = None;
                } else {
                    let n = c.len();
                    // Scanning summaries for huge containers would be slow; match on labels only past 20k items.
                    let idx: Vec<usize> =
                        (0..n).filter(|&i| c.filter_matches(i, &needle)).collect();
                    c.list.filtered = Some(idx);
                }
                c.list.selected = 0;
                c.list.offset = 0;
            }
            _ => {}
        }
    }

    /// Make sure the container's summaries exist for `range` (visible rows).
    pub fn ensure_summaries(
        c: &mut ContainerView,
        range: std::ops::Range<usize>,
        with_preview: Option<usize>,
        opts: &DisplayOpts,
    ) {
        for pos in range {
            let i = c.list.item_at(pos);
            if c.summaries.contains_key(&i) {
                continue;
            }
            let s = match c.mode {
                ContainerMode::StructElements => ChildSummary {
                    summary: format!("struct with {} fields", c.fields.len()),
                    inline: struct_elem_inline(&*c.node, i, &c.fields, opts),
                    preview: Vec::new(),
                    kind: Kind::Struct,
                    error: None,
                },
                _ => match c.child(i) {
                    Ok(child) => summarize_child(&*child, 0, opts),
                    Err(e) => ChildSummary {
                        summary: "?".into(),
                        inline: String::new(),
                        preview: Vec::new(),
                        kind: Kind::Unknown,
                        error: Some(e.to_string()),
                    },
                },
            };
            c.summaries.insert(i, s);
        }
        if let Some(i) = with_preview {
            let needs = c
                .summaries
                .get(&i)
                .map(|s| s.preview.is_empty())
                .unwrap_or(false);
            if needs {
                let preview = match c.mode {
                    ContainerMode::StructElements => {
                        struct_elem_preview(&*c.node, i, &c.fields, opts)
                    }
                    _ => match c.child(i) {
                        Ok(child) => preview_lines(&*child, 24, 60, opts),
                        Err(e) => vec![format!("error: {}", e)],
                    },
                };
                if let Some(s) = c.summaries.get_mut(&i) {
                    s.preview = if preview.is_empty() {
                        vec![String::new()]
                    } else {
                        preview
                    };
                }
            }
        }
    }
}

/// Wrapper so an `Arc<dyn Node>` can be handed back through the job channel.
struct SharedNode(NodeRef);

impl Node for SharedNode {
    fn class(&self) -> crate::model::Class {
        self.0.class()
    }
    fn shape(&self) -> Shape {
        self.0.shape()
    }
    fn is_complex(&self) -> bool {
        self.0.is_complex()
    }
    fn is_sparse(&self) -> bool {
        self.0.is_sparse()
    }
    fn kind(&self) -> Kind {
        self.0.kind()
    }
    fn info(&self) -> Vec<(String, String)> {
        self.0.info()
    }
    fn description(&self) -> Option<String> {
        self.0.description()
    }
    fn field_names(&self) -> Result<Vec<String>> {
        self.0.field_names()
    }
    fn field(&self, elem: usize, field: usize) -> Result<Box<dyn Node>> {
        self.0.field(elem, field)
    }
    fn cell(&self, index: usize) -> Result<Box<dyn Node>> {
        self.0.cell(index)
    }
    fn read_window(&self, window: &Window) -> Result<Vec<Scalar>> {
        self.0.read_window(window)
    }
    fn nnz(&self) -> usize {
        self.0.nnz()
    }
    fn sparse_entries(&self, range: std::ops::Range<usize>) -> Result<Vec<SparseEntry>> {
        self.0.sparse_entries(range)
    }
    fn inner(&self) -> Result<Option<Box<dyn Node>>> {
        self.0.inner()
    }
    fn text_row(&self, row: usize, page: &[usize]) -> Result<String> {
        self.0.text_row(row, page)
    }
}

fn page_index(page: &[usize], shape: &Shape) -> usize {
    let mut idx = 0;
    let mut stride = 1;
    for (i, &p) in page.iter().enumerate() {
        idx += p * stride;
        stride *= shape.dims()[i + 2];
    }
    idx
}

fn set_page_index(page: &mut [usize], shape: &Shape, mut idx: usize) {
    for (i, p) in page.iter_mut().enumerate() {
        let d = shape.dims()[i + 2].max(1);
        *p = idx % d;
        idx /= d;
    }
}

pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if !cur.is_empty() && cur.len() + 1 + word.len() > width {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

/// One-line rendering of a small node's value.
pub fn inline_value(node: &dyn Node, max: usize, opts: &DisplayOpts) -> String {
    let shape = node.shape();
    let class = node.class();
    match node.kind() {
        Kind::Numeric => {
            if shape.is_empty() {
                return "[]".into();
            }
            if shape.numel() <= 12 {
                match node.read_window(&Window::all(&shape)) {
                    Ok(vals) => {
                        let parts: Vec<String> = vals
                            .iter()
                            .map(|v| format_scalar(v, &class, opts))
                            .collect();
                        if shape.numel() == 1 {
                            truncate(&parts[0], max)
                        } else if shape.rows() == 1 || shape.cols() == 1 {
                            let sep = if shape.rows() == 1 { " " } else { "; " };
                            truncate(&format!("[{}]", parts.join(sep)), max)
                        } else {
                            let mut rows = Vec::new();
                            for r in 0..shape.rows() {
                                let row: Vec<String> = (0..shape.cols())
                                    .map(|c| parts[r + c * shape.rows()].clone())
                                    .collect();
                                rows.push(row.join(" "));
                            }
                            truncate(&format!("[{}]", rows.join("; ")), max)
                        }
                    }
                    Err(_) => String::new(),
                }
            } else {
                String::new()
            }
        }
        Kind::Char => {
            if shape.is_empty() {
                return "''".into();
            }
            if shape.rows() == 1 && shape.numel() <= 200 {
                match node.text_row(0, &[]) {
                    Ok(t) => truncate(&format!("'{}'", sanitize_text(&t)), max),
                    Err(_) => String::new(),
                }
            } else {
                String::new()
            }
        }
        Kind::Struct => {
            let n = node.field_names().map(|f| f.len()).unwrap_or(0);
            if shape.numel() == 1 {
                format!("{} field{}", n, if n == 1 { "" } else { "s" })
            } else {
                String::new()
            }
        }
        Kind::Cell => {
            if shape.is_empty() {
                "{}".into()
            } else {
                String::new()
            }
        }
        Kind::Sparse => format!("nnz = {}", node.nnz()),
        Kind::Function => String::new(),
        Kind::Opaque | Kind::Unknown => String::new(),
    }
}

pub fn summarize_child(node: &dyn Node, _depth: usize, opts: &DisplayOpts) -> ChildSummary {
    ChildSummary {
        summary: summarize(node),
        inline: inline_value(node, 40, opts),
        preview: Vec::new(),
        kind: node.kind(),
        error: None,
    }
}

fn struct_elem_inline(
    node: &dyn Node,
    elem: usize,
    fields: &[String],
    opts: &DisplayOpts,
) -> String {
    let mut parts = Vec::new();
    for (f, name) in fields.iter().enumerate().take(4) {
        if let Ok(child) = node.field(elem, f) {
            let v = inline_value(&*child, 20, opts);
            if !v.is_empty() {
                parts.push(format!("{}={}", name, v));
            } else {
                parts.push(format!("{}: {}", name, summarize(&*child)));
            }
        }
    }
    if fields.len() > 4 {
        parts.push("…".into());
    }
    truncate(&parts.join(", "), 60)
}

fn struct_elem_preview(
    node: &dyn Node,
    elem: usize,
    fields: &[String],
    opts: &DisplayOpts,
) -> Vec<String> {
    let mut lines = Vec::new();
    for (f, name) in fields.iter().enumerate().take(40) {
        match node.field(elem, f) {
            Ok(child) => {
                let v = inline_value(&*child, 30, opts);
                if v.is_empty() {
                    lines.push(format!("{}: {}", name, summarize(&*child)));
                } else {
                    lines.push(format!("{}: {}", name, v));
                }
            }
            Err(e) => lines.push(format!("{}: <error: {}>", name, e)),
        }
    }
    if fields.len() > 40 {
        lines.push(format!("… {} more fields", fields.len() - 40));
    }
    lines
}

/// Multi-line preview of a node for the side panel.
pub fn preview_lines(
    node: &dyn Node,
    max_lines: usize,
    width: usize,
    opts: &DisplayOpts,
) -> Vec<String> {
    let shape = node.shape();
    let class = node.class();
    let mut lines = vec![summarize(node)];
    match node.kind() {
        Kind::Numeric => {
            if shape.is_empty() {
                lines.push("[]".into());
                return lines;
            }
            let rows = shape.rows().min(max_lines.saturating_sub(2).max(1));
            let cols = shape.cols().min(8);
            let page = vec![0; shape.ndims().saturating_sub(2)];
            match node.read_window(&Window::page(0..rows, 0..cols, &page)) {
                Ok(vals) => {
                    let strs: Vec<String> = vals
                        .iter()
                        .map(|v| format_scalar(v, &class, opts))
                        .collect();
                    let mut col_w = vec![1usize; cols];
                    for c in 0..cols {
                        for r in 0..rows {
                            col_w[c] = col_w[c].max(strs[r + c * rows].chars().count());
                        }
                    }
                    for r in 0..rows {
                        let mut line = String::new();
                        for c in 0..cols {
                            let s = &strs[r + c * rows];
                            if line.len() + col_w[c] + 1 > width {
                                line.push('…');
                                break;
                            }
                            line.push_str(&format!("{:>w$} ", s, w = col_w[c]));
                        }
                        lines.push(line.trim_end().to_string());
                    }
                    if shape.rows() > rows || shape.cols() > cols || shape.ndims() > 2 {
                        lines.push("…".into());
                    }
                }
                Err(e) => lines.push(format!("error: {}", e)),
            }
        }
        Kind::Char => {
            if shape.is_empty() {
                lines.push("''".into());
                return lines;
            }
            let rows = shape.rows().min(max_lines.saturating_sub(2).max(1));
            let page = vec![0; shape.ndims().saturating_sub(2)];
            for r in 0..rows {
                match node.text_row(r, &page) {
                    Ok(t) => lines.push(truncate(&sanitize_text(&t), width)),
                    Err(e) => lines.push(format!("error: {}", e)),
                }
            }
            if shape.rows() > rows {
                lines.push("…".into());
            }
        }
        Kind::Cell => {
            let n = shape.numel();
            if n == 0 {
                lines.push("{}".into());
                return lines;
            }
            if n > 2000 {
                lines.push("(open to browse elements)".into());
                return lines;
            }
            for i in 0..n.min(max_lines.saturating_sub(2).max(1)) {
                let label = shape.format_subscripts(i);
                match node.cell(i) {
                    Ok(child) => {
                        let v = inline_value(&*child, width.saturating_sub(12), opts);
                        if v.is_empty() {
                            lines.push(format!(
                                "{{{}}} {}",
                                &label[1..label.len() - 1],
                                summarize(&*child)
                            ));
                        } else {
                            lines.push(format!("{{{}}} {}", &label[1..label.len() - 1], v));
                        }
                    }
                    Err(e) => lines.push(format!("{} error: {}", label, e)),
                }
            }
            if n > max_lines.saturating_sub(2) {
                lines.push("…".into());
            }
        }
        Kind::Struct | Kind::Function => {
            let fields = node.field_names().unwrap_or_default();
            if shape.numel() != 1 {
                lines.push(format!("fields: {}", truncate(&fields.join(", "), width)));
                return lines;
            }
            for (f, name) in fields.iter().enumerate().take(max_lines.saturating_sub(2)) {
                match node.field(0, f) {
                    Ok(child) => {
                        let v = inline_value(&*child, width.saturating_sub(name.len() + 3), opts);
                        if v.is_empty() {
                            lines.push(format!("{}: {}", name, summarize(&*child)));
                        } else {
                            lines.push(format!("{}: {}", name, v));
                        }
                    }
                    Err(e) => lines.push(format!("{}: <error: {}>", name, e)),
                }
            }
            if fields.len() > max_lines.saturating_sub(2) {
                lines.push("…".into());
            }
        }
        Kind::Sparse => {
            lines.push(format!("nnz = {}", node.nnz()));
            if let Ok(entries) = node.sparse_entries(0..max_lines.saturating_sub(3).max(1)) {
                for e in entries {
                    lines.push(format!(
                        "({},{}) {}",
                        e.row + 1,
                        e.col + 1,
                        format_scalar(&e.value, &class, opts)
                    ));
                }
            }
        }
        Kind::Opaque | Kind::Unknown => {
            if let Some(d) = node.description() {
                lines.extend(wrap(&d, width));
            }
        }
    }
    lines
}

/// Decode a char node fully into a single string (used for small text values).
#[allow(dead_code)]
pub fn char_to_string(node: &dyn Node) -> String {
    match node.read_window(&Window::all(&node.shape())) {
        Ok(v) => decode_chars(&v),
        Err(_) => String::new(),
    }
}
