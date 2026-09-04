//! Backend-independent model of a MATLAB array.
//!
//! Both the MAT v5 parser and the v7.3 (HDF5) reader expose their data through
//! the [`Node`] trait so the TUI never needs to know which file format it is
//! looking at. Everything here is designed around *lazy* access: a node is a
//! cheap handle that describes an array (class, shape, flags) and can hand out
//! children or small windows of values on demand, without materializing the
//! whole array.

use anyhow::Result;
use std::fmt;
use std::sync::Arc;

/// MATLAB class of an array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Class {
    Double,
    Single,
    Int8,
    Uint8,
    Int16,
    Uint16,
    Int32,
    Uint32,
    Int64,
    Uint64,
    Logical,
    Char,
    Cell,
    Struct,
    FunctionHandle,
    /// Legacy `@class` object (MAT v5 `mxOBJECT_CLASS`); stored like a struct.
    Object(String),
    /// New-style (MCOS) object such as `string`, `table`, `datetime`. Its
    /// contents live in the file's opaque subsystem and are not decodable here.
    Opaque(String),
    Unknown(String),
}

impl Class {
    pub fn name(&self) -> String {
        match self {
            Class::Double => "double".into(),
            Class::Single => "single".into(),
            Class::Int8 => "int8".into(),
            Class::Uint8 => "uint8".into(),
            Class::Int16 => "int16".into(),
            Class::Uint16 => "uint16".into(),
            Class::Int32 => "int32".into(),
            Class::Uint32 => "uint32".into(),
            Class::Int64 => "int64".into(),
            Class::Uint64 => "uint64".into(),
            Class::Logical => "logical".into(),
            Class::Char => "char".into(),
            Class::Cell => "cell".into(),
            Class::Struct => "struct".into(),
            Class::FunctionHandle => "function_handle".into(),
            Class::Object(name) => name.clone(),
            Class::Opaque(name) => name.clone(),
            Class::Unknown(name) => name.clone(),
        }
    }

    /// Parse a class name as it appears in a v7.3 `MATLAB_class` attribute.
    pub fn from_matlab_name(name: &str) -> Class {
        match name {
            "double" => Class::Double,
            "single" => Class::Single,
            "int8" => Class::Int8,
            "uint8" => Class::Uint8,
            "int16" => Class::Int16,
            "uint16" => Class::Uint16,
            "int32" => Class::Int32,
            "uint32" => Class::Uint32,
            "int64" => Class::Int64,
            "uint64" => Class::Uint64,
            "logical" => Class::Logical,
            "char" => Class::Char,
            "cell" => Class::Cell,
            "struct" => Class::Struct,
            "function_handle" => Class::FunctionHandle,
            other => Class::Opaque(other.to_string()),
        }
    }

    pub fn is_numeric(&self) -> bool {
        matches!(
            self,
            Class::Double
                | Class::Single
                | Class::Int8
                | Class::Uint8
                | Class::Int16
                | Class::Uint16
                | Class::Int32
                | Class::Uint32
                | Class::Int64
                | Class::Uint64
                | Class::Logical
        )
    }

    pub fn is_integer(&self) -> bool {
        matches!(
            self,
            Class::Int8
                | Class::Uint8
                | Class::Int16
                | Class::Uint16
                | Class::Int32
                | Class::Uint32
                | Class::Int64
                | Class::Uint64
        )
    }
}

impl fmt::Display for Class {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name())
    }
}

/// MATLAB dimensions (always at least two, column-major).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shape(pub Vec<usize>);

impl Shape {
    pub fn new(dims: Vec<usize>) -> Shape {
        let mut dims = dims;
        while dims.len() < 2 {
            dims.push(if dims.is_empty() { 0 } else { 1 });
        }
        Shape(dims)
    }

    pub fn dims(&self) -> &[usize] {
        &self.0
    }

    pub fn ndims(&self) -> usize {
        self.0.len()
    }

    pub fn numel(&self) -> usize {
        self.0.iter().product()
    }

    pub fn is_empty(&self) -> bool {
        self.numel() == 0
    }

    pub fn is_scalar(&self) -> bool {
        self.numel() == 1
    }

    pub fn is_vector(&self) -> bool {
        self.ndims() == 2 && (self.0[0] == 1 || self.0[1] == 1)
    }

    pub fn rows(&self) -> usize {
        self.0[0]
    }

    pub fn cols(&self) -> usize {
        self.0[1]
    }

    /// Product of all dimensions beyond the first two (number of 2-D pages).
    pub fn pages(&self) -> usize {
        self.0[2..].iter().product::<usize>().max(1)
    }

    /// Convert a zero-based linear (column-major) index to subscripts.
    pub fn subscripts(&self, mut linear: usize) -> Vec<usize> {
        let mut subs = Vec::with_capacity(self.ndims());
        for &d in &self.0 {
            if d == 0 {
                subs.push(0);
                continue;
            }
            subs.push(linear % d);
            linear /= d;
        }
        subs
    }

    /// Convert zero-based subscripts to a linear (column-major) index.
    pub fn linear_index(&self, subs: &[usize]) -> usize {
        let mut idx = 0;
        let mut stride = 1;
        for (i, &d) in self.0.iter().enumerate() {
            idx += subs.get(i).copied().unwrap_or(0) * stride;
            stride *= d;
        }
        idx
    }

    /// Human-readable one-based subscripts, e.g. `(3,1)` or `(2,1,4)`.
    pub fn format_subscripts(&self, linear: usize) -> String {
        let subs = self.subscripts(linear);
        let parts: Vec<String> = subs.iter().map(|s| (s + 1).to_string()).collect();
        format!("({})", parts.join(","))
    }
}

impl fmt::Display for Shape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<String> = self.0.iter().map(|d| d.to_string()).collect();
        f.write_str(&parts.join("x"))
    }
}

/// A single element value.
#[derive(Debug, Clone, PartialEq)]
pub enum Scalar {
    Float(f64),
    Int(i64),
    Uint(u64),
    Bool(bool),
    Complex(f64, f64),
    /// A character code unit (UTF-16 for MATLAB char arrays).
    Char(u32),
}

impl Scalar {
    pub fn as_f64(&self) -> f64 {
        match self {
            Scalar::Float(v) => *v,
            Scalar::Int(v) => *v as f64,
            Scalar::Uint(v) => *v as f64,
            Scalar::Bool(v) => {
                if *v {
                    1.0
                } else {
                    0.0
                }
            }
            Scalar::Complex(re, _) => *re,
            Scalar::Char(c) => *c as f64,
        }
    }

    pub fn is_zero(&self) -> bool {
        match self {
            Scalar::Float(v) => *v == 0.0,
            Scalar::Int(v) => *v == 0,
            Scalar::Uint(v) => *v == 0,
            Scalar::Bool(v) => !*v,
            Scalar::Complex(re, im) => *re == 0.0 && *im == 0.0,
            Scalar::Char(c) => *c == 0,
        }
    }
}

/// A rectangular window into an N-D array, in MATLAB (column-major) dims.
/// `start[i]..start[i]+len[i]` for each dimension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub start: Vec<usize>,
    pub len: Vec<usize>,
}

impl Window {
    /// A 2-D window on the page given by `page_subs` (subscripts for dims >= 2).
    pub fn page(
        rows: std::ops::Range<usize>,
        cols: std::ops::Range<usize>,
        page_subs: &[usize],
    ) -> Window {
        let mut start = vec![rows.start, cols.start];
        let mut len = vec![rows.len(), cols.len()];
        for &p in page_subs {
            start.push(p);
            len.push(1);
        }
        Window { start, len }
    }

    /// Every element of the array.
    pub fn all(shape: &Shape) -> Window {
        Window {
            start: vec![0; shape.ndims()],
            len: shape.dims().to_vec(),
        }
    }

    pub fn numel(&self) -> usize {
        self.len.iter().product()
    }

    /// Iterate over the absolute subscripts of the window's elements in
    /// column-major order (first subscript varying fastest).
    pub fn subscripts(&self) -> impl Iterator<Item = Vec<usize>> + '_ {
        let n = self.numel();
        (0..n).map(move |mut k| {
            let mut subs = Vec::with_capacity(self.len.len());
            for (s, l) in self.start.iter().zip(&self.len) {
                if *l == 0 {
                    subs.push(*s);
                    continue;
                }
                subs.push(s + k % l);
                k /= l;
            }
            subs
        })
    }

    /// Clamp the window to the given shape.
    pub fn clamp(mut self, shape: &Shape) -> Window {
        let dims = shape.dims();
        while self.start.len() < dims.len() {
            self.start.push(0);
            self.len.push(1);
        }
        for i in 0..self.start.len() {
            let d = dims.get(i).copied().unwrap_or(1);
            if self.start[i] >= d {
                self.start[i] = d.saturating_sub(1);
                self.len[i] = if d == 0 { 0 } else { self.len[i].min(1) };
            }
            self.len[i] = self.len[i].min(d - self.start[i].min(d));
        }
        self
    }
}

/// What kind of view a node wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Dense numeric or logical array; browse as a grid.
    Numeric,
    /// Character array; browse as text rows.
    Char,
    /// Cell array; browse elements as a list.
    Cell,
    /// Struct or struct array (and legacy objects); browse fields/elements.
    Struct,
    /// Sparse matrix; browse non-zero triplets.
    Sparse,
    /// Function handle; browse its underlying struct.
    Function,
    /// MCOS/opaque object; only a description and raw metadata are available.
    Opaque,
    Unknown,
}

/// A non-zero entry of a sparse matrix (zero-based row/col).
#[derive(Debug, Clone, PartialEq)]
pub struct SparseEntry {
    pub row: usize,
    pub col: usize,
    pub value: Scalar,
}

/// A lazy handle to a MATLAB array.
pub trait Node: Send + Sync {
    fn class(&self) -> Class;
    fn shape(&self) -> Shape;
    fn is_complex(&self) -> bool {
        false
    }
    fn is_sparse(&self) -> bool {
        false
    }
    fn kind(&self) -> Kind;

    /// Extra (label, value) pairs describing storage details for the info panel.
    fn info(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Short free-text description for opaque/unknown nodes.
    fn description(&self) -> Option<String> {
        None
    }

    // ---- struct-like access ----
    fn field_names(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
    /// Field `field` of element `elem` (both zero-based).
    fn field(&self, _elem: usize, _field: usize) -> Result<Box<dyn Node>> {
        anyhow::bail!("not a struct")
    }

    // ---- cell access ----
    fn cell(&self, _index: usize) -> Result<Box<dyn Node>> {
        anyhow::bail!("not a cell array")
    }

    // ---- dense numeric / char access ----
    /// Read the values in `window`, in column-major order over the window.
    fn read_window(&self, _window: &Window) -> Result<Vec<Scalar>> {
        anyhow::bail!("not a dense array")
    }

    // ---- sparse access ----
    fn nnz(&self) -> usize {
        0
    }
    /// Non-zero entries `range` (zero-based over the nnz entries, in
    /// compressed-sparse-column order).
    fn sparse_entries(&self, _range: std::ops::Range<usize>) -> Result<Vec<SparseEntry>> {
        anyhow::bail!("not a sparse matrix")
    }

    /// For function handles and opaque objects: the underlying data node.
    fn inner(&self) -> Result<Option<Box<dyn Node>>> {
        Ok(None)
    }

    /// Do any one-off work needed before children can be accessed cheaply
    /// (e.g. resolving object references). Called on a worker thread.
    fn prepare(&self) -> Result<()> {
        Ok(())
    }

    /// One row of a char array as text (dims >= 2 fixed by `page`).
    fn text_row(&self, row: usize, page: &[usize]) -> Result<String> {
        let shape = self.shape();
        let cols = shape.cols();
        let window = Window::page(row..row + 1, 0..cols, page);
        let units = self.read_window(&window)?;
        Ok(decode_chars(&units))
    }
}

/// Decode a sequence of char code units into a string (UTF-16 aware).
pub fn decode_chars(units: &[Scalar]) -> String {
    let codes: Vec<u32> = units
        .iter()
        .map(|s| match s {
            Scalar::Char(c) => *c,
            other => other.as_f64() as u32,
        })
        .collect();
    if codes.iter().all(|&c| c <= 0xFFFF) {
        char::decode_utf16(codes.iter().map(|&c| c as u16))
            .map(|r| r.unwrap_or('\u{FFFD}'))
            .collect()
    } else {
        codes
            .iter()
            .map(|&c| char::from_u32(c).unwrap_or('\u{FFFD}'))
            .collect()
    }
}

pub type NodeRef = Arc<dyn Node>;

/// Summary of a top-level variable, available without loading its data.
#[derive(Debug, Clone)]
pub struct VarInfo {
    pub name: String,
    pub class: Class,
    pub shape: Shape,
    pub is_complex: bool,
    pub is_sparse: bool,
    pub is_global: bool,
    /// Bytes this variable occupies in the file (compressed size if compressed).
    pub storage_bytes: u64,
    pub compressed: bool,
}

/// An open .mat file.
pub trait MatFile: Send + Sync {
    /// Short format description, e.g. "MAT v5 (compressed)" or "MAT v7.3 (HDF5)".
    fn format(&self) -> String;
    /// The 116-byte header text (or equivalent).
    fn header_text(&self) -> String;
    fn variables(&self) -> &[VarInfo];
    /// Load variable `index`. May be slow (e.g. inflating a compressed
    /// variable) and is expected to run on a worker thread. `cancel` is
    /// polled periodically; when set, the load may abort with an error.
    fn load(&self, index: usize, cancel: &std::sync::atomic::AtomicBool) -> Result<Box<dyn Node>>;
}

/// Produce a short one-line summary like `3x4 double` or `1x1 struct`.
pub fn summarize(node: &dyn Node) -> String {
    let shape = node.shape();
    let class = node.class();
    let mut s = format!("{} {}", shape, class.name());
    if node.is_sparse() {
        s = format!("{} sparse {}", shape, class.name());
    }
    if node.is_complex() {
        s.push_str(" complex");
    }
    s
}
