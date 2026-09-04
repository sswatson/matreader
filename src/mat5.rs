//! Lazy reader for MAT v5 files (the format written by `save -v6` and `save -v7`).
//!
//! The file is memory-mapped and indexed by walking the top-level data
//! elements. For a compressed (`-v7`) variable only the first few hundred
//! bytes are inflated to learn its name, class and dimensions; the full
//! payload is inflated when the variable is opened. Parsed arrays keep byte
//! ranges into the backing buffer rather than copies, so nested cells/structs
//! and numeric data are decoded on demand.

use crate::model::{Class, Kind, MatFile, Node, Scalar, Shape, SparseEntry, VarInfo, Window};
use anyhow::{anyhow, bail, Context, Result};
use flate2::{Decompress, FlushDecompress, Status};
use memmap2::Mmap;
use std::ops::Deref;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// MAT-file data types (mi*).
const MI_INT8: u32 = 1;
const MI_UINT8: u32 = 2;
const MI_INT16: u32 = 3;
const MI_UINT16: u32 = 4;
const MI_INT32: u32 = 5;
const MI_UINT32: u32 = 6;
const MI_SINGLE: u32 = 7;
const MI_DOUBLE: u32 = 9;
const MI_INT64: u32 = 12;
const MI_UINT64: u32 = 13;
const MI_MATRIX: u32 = 14;
const MI_COMPRESSED: u32 = 15;
const MI_UTF8: u32 = 16;
const MI_UTF16: u32 = 17;
const MI_UTF32: u32 = 18;

// Array classes (mx*).
const MX_CELL: u8 = 1;
const MX_STRUCT: u8 = 2;
const MX_OBJECT: u8 = 3;
const MX_CHAR: u8 = 4;
const MX_SPARSE: u8 = 5;
const MX_DOUBLE: u8 = 6;
const MX_SINGLE: u8 = 7;
const MX_INT8: u8 = 8;
const MX_UINT8: u8 = 9;
const MX_INT16: u8 = 10;
const MX_UINT16: u8 = 11;
const MX_INT32: u8 = 12;
const MX_UINT32: u8 = 13;
const MX_INT64: u8 = 14;
const MX_UINT64: u8 = 15;
const MX_FUNCTION: u8 = 16;
const MX_OPAQUE: u8 = 17;

const FLAG_COMPLEX: u32 = 0x0800;
const FLAG_GLOBAL: u32 = 0x0400;
const FLAG_LOGICAL: u32 = 0x0200;

fn mi_size(t: u32) -> Option<usize> {
    Some(match t {
        MI_INT8 | MI_UINT8 | MI_UTF8 => 1,
        MI_INT16 | MI_UINT16 | MI_UTF16 => 2,
        MI_INT32 | MI_UINT32 | MI_SINGLE | MI_UTF32 => 4,
        MI_DOUBLE | MI_INT64 | MI_UINT64 => 8,
        _ => return None,
    })
}

fn mi_name(t: u32) -> String {
    match t {
        MI_INT8 => "int8".into(),
        MI_UINT8 => "uint8".into(),
        MI_INT16 => "int16".into(),
        MI_UINT16 => "uint16".into(),
        MI_INT32 => "int32".into(),
        MI_UINT32 => "uint32".into(),
        MI_SINGLE => "single".into(),
        MI_DOUBLE => "double".into(),
        MI_INT64 => "int64".into(),
        MI_UINT64 => "uint64".into(),
        MI_MATRIX => "matrix".into(),
        MI_COMPRESSED => "compressed".into(),
        MI_UTF8 => "utf8".into(),
        MI_UTF16 => "utf16".into(),
        MI_UTF32 => "utf32".into(),
        other => format!("type {}", other),
    }
}

fn class_from_id(id: u8) -> Option<Class> {
    Some(match id {
        MX_DOUBLE => Class::Double,
        MX_SINGLE => Class::Single,
        MX_INT8 => Class::Int8,
        MX_UINT8 => Class::Uint8,
        MX_INT16 => Class::Int16,
        MX_UINT16 => Class::Uint16,
        MX_INT32 => Class::Int32,
        MX_UINT32 => Class::Uint32,
        MX_INT64 => Class::Int64,
        MX_UINT64 => Class::Uint64,
        MX_CHAR => Class::Char,
        MX_CELL => Class::Cell,
        MX_STRUCT => Class::Struct,
        MX_FUNCTION => Class::FunctionHandle,
        _ => return None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endian {
    Little,
    Big,
}

impl Endian {
    fn u16(self, b: &[u8]) -> u16 {
        let a = [b[0], b[1]];
        match self {
            Endian::Little => u16::from_le_bytes(a),
            Endian::Big => u16::from_be_bytes(a),
        }
    }
    fn u32(self, b: &[u8]) -> u32 {
        let a = [b[0], b[1], b[2], b[3]];
        match self {
            Endian::Little => u32::from_le_bytes(a),
            Endian::Big => u32::from_be_bytes(a),
        }
    }
    fn u64(self, b: &[u8]) -> u64 {
        let a: [u8; 8] = b[..8].try_into().unwrap();
        match self {
            Endian::Little => u64::from_le_bytes(a),
            Endian::Big => u64::from_be_bytes(a),
        }
    }
    fn i16(self, b: &[u8]) -> i16 {
        self.u16(b) as i16
    }
    fn i32(self, b: &[u8]) -> i32 {
        self.u32(b) as i32
    }
    fn i64(self, b: &[u8]) -> i64 {
        self.u64(b) as i64
    }
    fn f32(self, b: &[u8]) -> f32 {
        f32::from_bits(self.u32(b))
    }
    fn f64(self, b: &[u8]) -> f64 {
        f64::from_bits(self.u64(b))
    }
}

/// Bytes backing parsed arrays: the file mapping or an inflated variable.
pub enum Buffer {
    Mmap(Mmap),
    Owned(Vec<u8>),
}

impl Deref for Buffer {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Buffer::Mmap(m) => m,
            Buffer::Owned(v) => v,
        }
    }
}

/// A parsed data-element tag.
#[derive(Debug, Clone, Copy)]
struct Tag {
    mi_type: u32,
    nbytes: usize,
    /// Offset of the payload.
    data: usize,
    /// Offset of the next element (payload padded to 8 bytes).
    next: usize,
}

struct Cursor<'a> {
    buf: &'a [u8],
    endian: Endian,
    /// Accept elements whose payload extends past the buffer (used when
    /// parsing the header of a partially inflated variable).
    lenient: bool,
}

impl<'a> Cursor<'a> {
    fn tag(&self, pos: usize) -> Result<Tag> {
        if pos + 8 > self.buf.len() {
            bail!("truncated element tag at offset {}", pos);
        }
        let word = self.endian.u32(&self.buf[pos..]);
        if word & 0xFFFF_0000 != 0 {
            // Small data element: type and byte count share the first word.
            let mi_type = word & 0xFFFF;
            let nbytes = (word >> 16) as usize;
            if nbytes > 4 {
                bail!("invalid small data element at offset {}", pos);
            }
            Ok(Tag {
                mi_type,
                nbytes,
                data: pos + 4,
                next: pos + 8,
            })
        } else {
            let nbytes = self.endian.u32(&self.buf[pos + 4..]) as usize;
            let data = pos + 8;
            if data + nbytes > self.buf.len() && !self.lenient {
                bail!(
                    "element at offset {} claims {} bytes but only {} remain",
                    pos,
                    nbytes,
                    self.buf.len().saturating_sub(data)
                );
            }
            Ok(Tag {
                mi_type: word,
                nbytes,
                data,
                next: data + nbytes.div_ceil(8) * 8,
            })
        }
    }

    fn bytes(&self, tag: &Tag) -> &'a [u8] {
        let end = (tag.data + tag.nbytes).min(self.buf.len());
        &self.buf[tag.data.min(end)..end]
    }

    fn expect(&self, pos: usize, want: &[u32], what: &str) -> Result<Tag> {
        let tag = self.tag(pos)?;
        if !want.contains(&tag.mi_type) {
            bail!(
                "expected {} at offset {}, found {}",
                what,
                pos,
                mi_name(tag.mi_type)
            );
        }
        Ok(tag)
    }

    fn i32s(&self, tag: &Tag) -> Vec<i32> {
        self.bytes(tag)
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| self.endian.i32(c))
            .collect()
    }

    fn text(&self, tag: &Tag) -> String {
        let b = self.bytes(tag);
        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        String::from_utf8_lossy(&b[..end]).into_owned()
    }
}

/// A numeric payload: `count` elements of `mi_type` at `offset`.
#[derive(Debug, Clone, Copy)]
pub struct Data {
    mi_type: u32,
    offset: usize,
    nbytes: usize,
}

impl Data {
    fn from_tag(t: &Tag) -> Data {
        Data {
            mi_type: t.mi_type,
            offset: t.data,
            nbytes: t.nbytes,
        }
    }
    fn count(&self) -> usize {
        mi_size(self.mi_type).map(|s| self.nbytes / s).unwrap_or(0)
    }
}

#[derive(Debug, Clone)]
pub enum Body {
    Numeric {
        real: Data,
        imag: Option<Data>,
    },
    Sparse {
        ir: Data,
        jc: Data,
        real: Data,
        imag: Option<Data>,
    },
    Char {
        data: Data,
    },
    /// Offsets of the nested `miMATRIX` elements, in column-major order.
    Cell {
        elems: Vec<usize>,
    },
    /// `elems[e * nfields + f]` is the offset of field `f` of element `e`.
    Struct {
        fields: Vec<String>,
        elems: Vec<usize>,
    },
    Function {
        inner: usize,
    },
    Opaque {
        type_system: String,
        class_name: String,
        inner: usize,
    },
    /// A zero-length `miMATRIX` (used for `[]` inside cells).
    Empty,
    Unknown,
}

/// A parsed array header plus lazily-resolved body.
#[derive(Debug, Clone)]
pub struct Array {
    pub class: Class,
    pub class_id: u8,
    pub flags: u32,
    pub nzmax: u32,
    pub dims: Vec<usize>,
    pub name: String,
    pub body: Body,
    /// Total payload size of this element in the buffer.
    pub nbytes: usize,
}

impl Array {
    fn is_complex(&self) -> bool {
        self.flags & FLAG_COMPLEX != 0
    }
    fn is_logical(&self) -> bool {
        self.flags & FLAG_LOGICAL != 0
    }
    fn is_global(&self) -> bool {
        self.flags & FLAG_GLOBAL != 0
    }
    fn shape(&self) -> Shape {
        Shape::new(self.dims.clone())
    }
}

/// Parse a `miMATRIX` element at `pos`. With `header_only`, nested element
/// lists (cells/structs) are not walked, which keeps it cheap for indexing.
fn parse_array(cur: &Cursor, pos: usize, header_only: bool) -> Result<Array> {
    let tag = cur.expect(pos, &[MI_MATRIX], "miMATRIX")?;
    let end = tag.data + tag.nbytes;
    if tag.nbytes == 0 {
        return Ok(Array {
            class: Class::Double,
            class_id: MX_DOUBLE,
            flags: 0,
            nzmax: 0,
            dims: vec![0, 0],
            name: String::new(),
            body: Body::Empty,
            nbytes: 0,
        });
    }
    let sub = Cursor {
        buf: &cur.buf[..end.min(cur.buf.len())],
        endian: cur.endian,
        lenient: cur.lenient,
    };
    let mut p = tag.data;

    // Array flags: two uint32 words.
    let ft = sub.expect(p, &[MI_UINT32], "array flags")?;
    if ft.nbytes < 8 {
        bail!("array flags element too short at offset {}", p);
    }
    let flags = sub.endian.u32(sub.bytes(&ft));
    let nzmax = sub.endian.u32(&sub.bytes(&ft)[4..]);
    let class_id = (flags & 0xFF) as u8;
    p = ft.next;

    if class_id == MX_OPAQUE {
        // Opaque objects: name, type system, class name, then a metadata matrix.
        // (They carry no dimensions element; dims come from the metadata.)
        let nt = sub.expect(p, &[MI_INT8, MI_UINT8, MI_UTF8], "opaque array name")?;
        let name = sub.text(&nt);
        p = nt.next;
        let tst = sub.expect(p, &[MI_INT8, MI_UINT8, MI_UTF8], "opaque type system")?;
        let type_system = sub.text(&tst);
        p = tst.next;
        let cnt = sub.expect(p, &[MI_INT8, MI_UINT8, MI_UTF8], "opaque class name")?;
        let class_name = sub.text(&cnt);
        p = cnt.next;
        let inner = p;
        let mut dims = vec![1, 1];
        if let Ok(meta) = parse_array(&sub, inner, true) {
            if let Body::Numeric { real, .. } = &meta.body {
                if real.mi_type == MI_UINT32
                    && real.count() >= 2
                    && real.offset + real.nbytes <= sub.buf.len()
                {
                    let words: Vec<u32> = sub.buf[real.offset..real.offset + real.nbytes]
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|c| sub.endian.u32(c))
                        .collect();
                    if words[0] == 0xDD00_0000 {
                        let nd = words[1] as usize;
                        if nd >= 1 && words.len() >= 2 + nd {
                            dims = words[2..2 + nd].iter().map(|&d| d as usize).collect();
                        }
                    }
                }
            }
        }
        return Ok(Array {
            class: Class::Opaque(class_name.clone()),
            class_id,
            flags,
            nzmax,
            dims,
            name,
            body: Body::Opaque {
                type_system,
                class_name,
                inner,
            },
            nbytes: tag.nbytes,
        });
    }

    // Dimensions.
    let dt = sub.expect(p, &[MI_INT32, MI_UINT32], "dimensions")?;
    let dims: Vec<usize> = sub.i32s(&dt).iter().map(|&d| d.max(0) as usize).collect();
    p = dt.next;

    // Name (may be an empty small element for nested arrays).
    let nt = sub.expect(p, &[MI_INT8, MI_UINT8, MI_UTF8], "array name")?;
    let name = sub.text(&nt);
    p = nt.next;

    let numel: usize = dims.iter().product();
    let logical = flags & FLAG_LOGICAL != 0;

    let (class, body) = match class_id {
        MX_CELL => {
            let elems = if header_only {
                Vec::new()
            } else {
                walk_elements(&sub, p, end, numel)?
            };
            (Class::Cell, Body::Cell { elems })
        }
        MX_STRUCT | MX_OBJECT => {
            let mut class = Class::Struct;
            if class_id == MX_OBJECT {
                let ct = sub.expect(p, &[MI_INT8, MI_UINT8, MI_UTF8], "object class name")?;
                class = Class::Object(sub.text(&ct));
                p = ct.next;
            }
            let lt = sub.expect(p, &[MI_INT32], "field name length")?;
            let field_len = sub.i32s(&lt).first().copied().unwrap_or(0).max(0) as usize;
            p = lt.next;
            let ft = sub.expect(p, &[MI_INT8, MI_UINT8, MI_UTF8], "field names")?;
            let names_bytes = sub.bytes(&ft);
            let mut fields = Vec::new();
            if field_len > 0 {
                for chunk in names_bytes.chunks(field_len) {
                    let e = chunk.iter().position(|&c| c == 0).unwrap_or(chunk.len());
                    fields.push(String::from_utf8_lossy(&chunk[..e]).into_owned());
                }
            }
            p = ft.next;
            let elems = if header_only {
                Vec::new()
            } else {
                walk_elements(&sub, p, end, numel * fields.len())?
            };
            (class, Body::Struct { fields, elems })
        }
        MX_CHAR => {
            let data = if p < end {
                Data::from_tag(&sub.tag(p)?)
            } else {
                Data {
                    mi_type: MI_UINT16,
                    offset: p,
                    nbytes: 0,
                }
            };
            (Class::Char, Body::Char { data })
        }
        MX_SPARSE => {
            let irt = sub.expect(p, &[MI_INT32, MI_UINT32], "sparse row indices")?;
            p = irt.next;
            let jct = sub.expect(p, &[MI_INT32, MI_UINT32], "sparse column pointers")?;
            p = jct.next;
            let prt = sub.tag(p)?;
            p = prt.next;
            let imag = if flags & FLAG_COMPLEX != 0 && p < end {
                Some(Data::from_tag(&sub.tag(p)?))
            } else {
                None
            };
            let class = if logical {
                Class::Logical
            } else {
                Class::Double
            };
            (
                class,
                Body::Sparse {
                    ir: Data::from_tag(&irt),
                    jc: Data::from_tag(&jct),
                    real: Data::from_tag(&prt),
                    imag,
                },
            )
        }
        MX_FUNCTION => (Class::FunctionHandle, Body::Function { inner: p }),
        id if class_from_id(id).is_some() => {
            let real = if p < end {
                let t = sub.tag(p)?;
                p = t.next;
                Data::from_tag(&t)
            } else {
                Data {
                    mi_type: MI_DOUBLE,
                    offset: p,
                    nbytes: 0,
                }
            };
            let imag = if flags & FLAG_COMPLEX != 0 && p < end {
                Some(Data::from_tag(&sub.tag(p)?))
            } else {
                None
            };
            let class = if logical {
                Class::Logical
            } else {
                class_from_id(id).unwrap()
            };
            (class, Body::Numeric { real, imag })
        }
        other => (Class::Unknown(format!("class {}", other)), Body::Unknown),
    };

    Ok(Array {
        class,
        class_id,
        flags,
        nzmax,
        dims,
        name,
        body,
        nbytes: tag.nbytes,
    })
}

/// Record the offsets of up to `count` consecutive elements starting at `pos`.
fn walk_elements(cur: &Cursor, mut pos: usize, end: usize, count: usize) -> Result<Vec<usize>> {
    let mut elems = Vec::with_capacity(count.min(1 << 20));
    while elems.len() < count && pos + 8 <= end {
        let t = cur.tag(pos)?;
        elems.push(pos);
        pos = t.next;
    }
    Ok(elems)
}

/// Inflate up to `max_out` bytes from a zlib stream.
fn inflate_prefix(input: &[u8], max_out: usize) -> Result<Vec<u8>> {
    inflate(input, Some(max_out), &AtomicBool::new(false))
}

/// Inflate an entire zlib stream, checking `cancel` between chunks.
fn inflate_all(input: &[u8], cancel: &AtomicBool) -> Result<Vec<u8>> {
    inflate(input, None, cancel)
}

/// Streaming zlib inflate through a fixed scratch buffer. (Decompressing
/// straight into a huge `Vec`'s spare capacity is pathologically slow with
/// flate2, which zero-fills the spare capacity on every call.)
fn inflate(input: &[u8], max_out: Option<usize>, cancel: &AtomicBool) -> Result<Vec<u8>> {
    let mut d = Decompress::new(true);
    let limit = max_out.unwrap_or(usize::MAX);
    let mut out: Vec<u8> = Vec::with_capacity(match max_out {
        Some(m) => m.min(1 << 16),
        None => input.len().saturating_mul(2).min(1 << 30),
    });
    let mut scratch = vec![0u8; (1usize << 20).min(limit.max(1024))];
    let mut in_pos = 0usize;
    while out.len() < limit {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        let before_in = d.total_in();
        let before_out = d.total_out();
        let status = d
            .decompress(&input[in_pos..], &mut scratch, FlushDecompress::None)
            .context("zlib inflate failed")?;
        in_pos += (d.total_in() - before_in) as usize;
        let produced = (d.total_out() - before_out) as usize;
        let take = produced.min(limit - out.len());
        out.extend_from_slice(&scratch[..take]);
        if status == Status::StreamEnd {
            break;
        }
        if produced == 0 && d.total_in() == before_in {
            // No progress (truncated stream); keep what we have.
            break;
        }
    }
    Ok(out)
}

#[derive(Debug, Clone)]
struct Entry {
    /// Offset of the top-level element tag.
    pos: usize,
    /// Payload offset and size.
    data: usize,
    nbytes: usize,
    compressed: bool,
}

pub struct Mat5File {
    buf: Arc<Buffer>,
    endian: Endian,
    header_text: String,
    version: u16,
    has_subsystem: bool,
    entries: Vec<Entry>,
    vars: Vec<VarInfo>,
    any_compressed: bool,
}

impl Mat5File {
    pub fn open(path: &Path) -> Result<Mat5File> {
        let file =
            std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mmap =
            unsafe { Mmap::map(&file) }.with_context(|| format!("mapping {}", path.display()))?;
        let buf = Arc::new(Buffer::Mmap(mmap));
        Self::from_buffer(buf)
    }

    fn from_buffer(buf: Arc<Buffer>) -> Result<Mat5File> {
        let bytes: &[u8] = &buf;
        if bytes.len() < 128 {
            bail!("file is too short to be a MAT v5 file");
        }
        let header_text = String::from_utf8_lossy(&bytes[..116])
            .trim_end_matches(['\0', ' '])
            .to_string();
        if !header_text.starts_with("MATLAB") {
            if bytes[..4].iter().all(|&b| b == 0 || b < 0x20) {
                bail!("this looks like a MAT v4 file, which is not supported");
            }
            bail!("not a MAT-file (missing MATLAB header)");
        }
        let endian = match &bytes[126..128] {
            b"IM" => Endian::Little,
            b"MI" => Endian::Big,
            other => bail!("unrecognized endian indicator {:?}", other),
        };
        let version = endian.u16(&bytes[124..126]);
        let has_subsystem = bytes[116..124].iter().any(|&b| b != 0 && b != b' ');
        let subsys_offset = if has_subsystem {
            Some(endian.u64(&bytes[116..124]) as usize)
        } else {
            None
        };

        let cur = Cursor {
            buf: bytes,
            endian,
            lenient: false,
        };
        let mut entries = Vec::new();
        let mut vars = Vec::new();
        let mut any_compressed = false;
        let mut pos = 128usize;
        while pos + 8 <= bytes.len() {
            let tag = match cur.tag(pos) {
                Ok(t) => t,
                Err(_) => break,
            };
            match tag.mi_type {
                MI_COMPRESSED => {
                    any_compressed = true;
                    let input = cur.bytes(&tag);
                    let header = match inflate_prefix(input, 1024)
                        .and_then(|b| parse_header_from(&b, endian))
                        .or_else(|_| {
                            inflate_prefix(input, 1 << 16)
                                .and_then(|b| parse_header_from(&b, endian))
                        }) {
                        Ok(h) => h,
                        Err(e) => {
                            vars.push(VarInfo {
                                name: format!("<unreadable element at {}: {}>", pos, e),
                                class: Class::Unknown("?".into()),
                                shape: Shape::new(vec![0, 0]),
                                is_complex: false,
                                is_sparse: false,
                                is_global: false,
                                storage_bytes: tag.nbytes as u64,
                                compressed: true,
                            });
                            entries.push(Entry {
                                pos,
                                data: tag.data,
                                nbytes: tag.nbytes,
                                compressed: true,
                            });
                            pos = advance_after_compressed(&cur, tag.data + tag.nbytes);
                            continue;
                        }
                    };
                    vars.push(var_info(&header, tag.nbytes as u64, true));
                    entries.push(Entry {
                        pos,
                        data: tag.data,
                        nbytes: tag.nbytes,
                        compressed: true,
                    });
                    // Compressed elements are not padded to 8 bytes by MATLAB.
                    pos = advance_after_compressed(&cur, tag.data + tag.nbytes);
                }
                MI_MATRIX if Some(pos) == subsys_offset => {
                    // The MCOS object subsystem blob; not a user variable.
                    pos = tag.next;
                }
                MI_MATRIX => {
                    match parse_array(&cur, pos, true) {
                        Ok(header) => vars.push(var_info(&header, tag.nbytes as u64, false)),
                        Err(e) => vars.push(VarInfo {
                            name: format!("<unreadable element at {}: {}>", pos, e),
                            class: Class::Unknown("?".into()),
                            shape: Shape::new(vec![0, 0]),
                            is_complex: false,
                            is_sparse: false,
                            is_global: false,
                            storage_bytes: tag.nbytes as u64,
                            compressed: false,
                        }),
                    }
                    entries.push(Entry {
                        pos,
                        data: tag.data,
                        nbytes: tag.nbytes,
                        compressed: false,
                    });
                    pos = tag.next;
                }
                _ => {
                    // Unknown top-level element (e.g. subsystem data); skip it.
                    pos = tag.next;
                }
            }
        }

        Ok(Mat5File {
            buf,
            endian,
            header_text,
            version,
            has_subsystem,
            entries,
            vars,
            any_compressed,
        })
    }
}

/// After a compressed element, find the next tag: MATLAB does not pad these,
/// but some writers do, so accept either.
fn advance_after_compressed(cur: &Cursor, pos: usize) -> usize {
    if pos.is_multiple_of(8) || pos + 8 > cur.buf.len() {
        return pos;
    }
    if let Ok(t) = cur.tag(pos) {
        if matches!(t.mi_type, MI_COMPRESSED | MI_MATRIX) {
            return pos;
        }
    }
    pos.div_ceil(8) * 8
}

fn parse_header_from(inflated: &[u8], endian: Endian) -> Result<Array> {
    // The inflated bytes are a (possibly truncated) miMATRIX element; parse the
    // header with a cursor that tolerates payloads running past the buffer.
    if inflated.len() < 8 {
        bail!("inflated prefix too short");
    }
    let word = endian.u32(&inflated[0..4]);
    if word != MI_MATRIX {
        bail!(
            "compressed element does not contain a matrix (type {})",
            mi_name(word)
        );
    }
    let cur = Cursor {
        buf: inflated,
        endian,
        lenient: true,
    };
    parse_array(&cur, 0, true)
}

fn var_info(header: &Array, storage_bytes: u64, compressed: bool) -> VarInfo {
    VarInfo {
        name: header.name.clone(),
        class: header.class.clone(),
        shape: header.shape(),
        is_complex: header.is_complex(),
        is_sparse: header.class_id == MX_SPARSE,
        is_global: header.is_global(),
        storage_bytes,
        compressed,
    }
}

impl MatFile for Mat5File {
    fn format(&self) -> String {
        let v = if self.version == 0x0100 {
            if self.any_compressed {
                "MAT v7 (v5 format, compressed)"
            } else {
                "MAT v6 (v5 format)"
            }
        } else {
            "MAT v5 format"
        };
        let mut s = v.to_string();
        if self.endian == Endian::Big {
            s.push_str(", big-endian");
        }
        if self.has_subsystem {
            s.push_str(", with object subsystem");
        }
        s
    }

    fn header_text(&self) -> String {
        self.header_text.clone()
    }

    fn variables(&self) -> &[VarInfo] {
        &self.vars
    }

    fn load(&self, index: usize, cancel: &AtomicBool) -> Result<Box<dyn Node>> {
        let entry = self
            .entries
            .get(index)
            .ok_or_else(|| anyhow!("no variable at index {}", index))?;
        if entry.compressed {
            let input = &self.buf[entry.data..entry.data + entry.nbytes];
            let inflated = inflate_all(input, cancel)?;
            let buf = Arc::new(Buffer::Owned(inflated));
            let cur = Cursor {
                buf: &buf,
                endian: self.endian,
                lenient: false,
            };
            let arr = parse_array(&cur, 0, false)?;
            Ok(Box::new(Mat5Node {
                buf: buf.clone(),
                endian: self.endian,
                arr,
                compressed_bytes: Some(entry.nbytes as u64),
            }))
        } else {
            let cur = Cursor {
                buf: &self.buf,
                endian: self.endian,
                lenient: false,
            };
            let arr = parse_array(&cur, entry.pos, false)?;
            Ok(Box::new(Mat5Node {
                buf: self.buf.clone(),
                endian: self.endian,
                arr,
                compressed_bytes: None,
            }))
        }
    }
}

/// A lazily-decoded array inside a v5 buffer.
pub struct Mat5Node {
    buf: Arc<Buffer>,
    endian: Endian,
    arr: Array,
    compressed_bytes: Option<u64>,
}

enum Num {
    F(f64),
    I(i64),
    U(u64),
}

impl Mat5Node {
    fn cursor(&self) -> Cursor<'_> {
        Cursor {
            buf: &self.buf,
            endian: self.endian,
            lenient: false,
        }
    }

    fn child(&self, pos: usize) -> Result<Box<dyn Node>> {
        let arr = parse_array(&self.cursor(), pos, false)?;
        Ok(Box::new(Mat5Node {
            buf: self.buf.clone(),
            endian: self.endian,
            arr,
            compressed_bytes: None,
        }))
    }

    fn raw(&self, d: &Data, i: usize) -> Option<Num> {
        let size = mi_size(d.mi_type)?;
        let off = d.offset + i * size;
        if off + size > d.offset + d.nbytes || off + size > self.buf.len() {
            return None;
        }
        let b = &self.buf[off..off + size];
        let e = self.endian;
        Some(match d.mi_type {
            MI_INT8 => Num::I(b[0] as i8 as i64),
            MI_UINT8 | MI_UTF8 => Num::U(b[0] as u64),
            MI_INT16 => Num::I(e.i16(b) as i64),
            MI_UINT16 | MI_UTF16 => Num::U(e.u16(b) as u64),
            MI_INT32 => Num::I(e.i32(b) as i64),
            MI_UINT32 | MI_UTF32 => Num::U(e.u32(b) as u64),
            MI_INT64 => Num::I(e.i64(b)),
            MI_UINT64 => Num::U(e.u64(b)),
            MI_SINGLE => Num::F(e.f32(b) as f64),
            MI_DOUBLE => Num::F(e.f64(b)),
            _ => return None,
        })
    }

    fn num_to_f64(n: &Num) -> f64 {
        match n {
            Num::F(v) => *v,
            Num::I(v) => *v as f64,
            Num::U(v) => *v as f64,
        }
    }

    fn scalar_at(&self, real: &Data, imag: Option<&Data>, i: usize) -> Scalar {
        let re = self.raw(real, i);
        let class = &self.arr.class;
        if let Some(im) = imag {
            let imv = self
                .raw(im, i)
                .map(|n| Self::num_to_f64(&n))
                .unwrap_or(f64::NAN);
            let rev = re.map(|n| Self::num_to_f64(&n)).unwrap_or(f64::NAN);
            return Scalar::Complex(rev, imv);
        }
        let re = match re {
            Some(n) => n,
            None => return Scalar::Float(f64::NAN),
        };
        match class {
            Class::Logical => Scalar::Bool(Self::num_to_f64(&re) != 0.0),
            Class::Double | Class::Single => Scalar::Float(Self::num_to_f64(&re)),
            Class::Int8 | Class::Int16 | Class::Int32 | Class::Int64 => Scalar::Int(match re {
                Num::I(v) => v,
                Num::U(v) => v as i64,
                Num::F(v) => v as i64,
            }),
            Class::Uint8 | Class::Uint16 | Class::Uint32 | Class::Uint64 => {
                Scalar::Uint(match re {
                    Num::U(v) => v,
                    Num::I(v) => v as u64,
                    Num::F(v) => v as u64,
                })
            }
            Class::Char => {
                let code = match re {
                    Num::U(v) => v as u32,
                    Num::I(v) => v as u32,
                    Num::F(v) => v as u32,
                };
                Scalar::Char(code)
            }
            _ => Scalar::Float(Self::num_to_f64(&re)),
        }
    }

    fn jc_values(&self, jc: &Data) -> Vec<usize> {
        (0..jc.count())
            .map(|i| {
                self.raw(jc, i)
                    .map(|n| Self::num_to_f64(&n) as usize)
                    .unwrap_or(0)
            })
            .collect()
    }
}

impl Node for Mat5Node {
    fn class(&self) -> Class {
        self.arr.class.clone()
    }

    fn shape(&self) -> Shape {
        self.arr.shape()
    }

    fn is_complex(&self) -> bool {
        self.arr.is_complex()
    }

    fn is_sparse(&self) -> bool {
        matches!(self.arr.body, Body::Sparse { .. })
    }

    fn kind(&self) -> Kind {
        match &self.arr.body {
            Body::Numeric { .. } => Kind::Numeric,
            Body::Empty => Kind::Numeric,
            Body::Sparse { .. } => Kind::Sparse,
            Body::Char { .. } => Kind::Char,
            Body::Cell { .. } => Kind::Cell,
            Body::Struct { .. } => Kind::Struct,
            Body::Function { .. } => Kind::Function,
            Body::Opaque { .. } => Kind::Opaque,
            Body::Unknown => Kind::Unknown,
        }
    }

    fn info(&self) -> Vec<(String, String)> {
        let mut v = Vec::new();
        if let Some(c) = self.compressed_bytes {
            v.push(("compressed size".into(), crate::format::format_bytes(c)));
            v.push((
                "inflated size".into(),
                crate::format::format_bytes(self.arr.nbytes as u64 + 8),
            ));
        } else {
            v.push((
                "size in file".into(),
                crate::format::format_bytes(self.arr.nbytes as u64 + 8),
            ));
        }
        match &self.arr.body {
            Body::Numeric { real, imag } => {
                v.push(("stored as".into(), mi_name(real.mi_type)));
                if let Some(im) = imag {
                    v.push(("imaginary stored as".into(), mi_name(im.mi_type)));
                }
            }
            Body::Char { data } => v.push(("encoding".into(), mi_name(data.mi_type))),
            Body::Sparse { real, .. } => {
                v.push(("nzmax".into(), self.arr.nzmax.to_string()));
                v.push(("values stored as".into(), mi_name(real.mi_type)));
            }
            Body::Opaque {
                type_system,
                class_name,
                ..
            } => {
                v.push(("type system".into(), type_system.clone()));
                v.push(("class".into(), class_name.clone()));
            }
            _ => {}
        }
        if self.arr.is_global() {
            v.push(("global".into(), "yes".into()));
        }
        if self.arr.is_logical() && self.arr.class != Class::Logical {
            v.push(("logical flag".into(), "set".into()));
        }
        v
    }

    fn description(&self) -> Option<String> {
        match &self.arr.body {
            Body::Opaque {
                type_system,
                class_name,
                ..
            } => Some(format!(
                "{} object of class '{}'. Its contents are stored in the file's object \
                 subsystem, which matreader does not decode; only the object metadata is shown.",
                type_system, class_name
            )),
            Body::Unknown => Some(format!("Unrecognized array class {}", self.arr.class_id)),
            _ => None,
        }
    }

    fn field_names(&self) -> Result<Vec<String>> {
        match &self.arr.body {
            Body::Struct { fields, .. } => Ok(fields.clone()),
            Body::Function { inner } => self.child(*inner)?.field_names(),
            _ => Ok(Vec::new()),
        }
    }

    fn field(&self, elem: usize, field: usize) -> Result<Box<dyn Node>> {
        match &self.arr.body {
            Body::Struct { fields, elems } => {
                let idx = elem * fields.len() + field;
                let pos = *elems
                    .get(idx)
                    .ok_or_else(|| anyhow!("field {} of element {} is missing", field, elem))?;
                self.child(pos)
            }
            Body::Function { inner } => self.child(*inner)?.field(elem, field),
            _ => bail!("not a struct"),
        }
    }

    fn cell(&self, index: usize) -> Result<Box<dyn Node>> {
        match &self.arr.body {
            Body::Cell { elems } => {
                let pos = *elems
                    .get(index)
                    .ok_or_else(|| anyhow!("cell element {} is missing", index))?;
                self.child(pos)
            }
            _ => bail!("not a cell array"),
        }
    }

    fn read_window(&self, window: &Window) -> Result<Vec<Scalar>> {
        let shape = self.shape();
        let (real, imag) = match &self.arr.body {
            Body::Numeric { real, imag } => (real, imag.as_ref()),
            Body::Char { data } => (data, None),
            Body::Empty => return Ok(Vec::new()),
            _ => bail!("not a dense array"),
        };
        let mut out = Vec::with_capacity(window.numel());
        for subs in window.subscripts() {
            let i = shape.linear_index(&subs);
            out.push(self.scalar_at(real, imag, i));
        }
        Ok(out)
    }

    fn nnz(&self) -> usize {
        match &self.arr.body {
            Body::Sparse { jc, .. } => {
                let n = jc.count();
                if n == 0 {
                    0
                } else {
                    self.raw(jc, n - 1)
                        .map(|v| Self::num_to_f64(&v) as usize)
                        .unwrap_or(0)
                }
            }
            _ => 0,
        }
    }

    fn sparse_entries(&self, range: std::ops::Range<usize>) -> Result<Vec<SparseEntry>> {
        let (ir, jc, real, imag) = match &self.arr.body {
            Body::Sparse { ir, jc, real, imag } => (ir, jc, real, imag.as_ref()),
            _ => bail!("not a sparse matrix"),
        };
        let jcv = self.jc_values(jc);
        let nnz = self.nnz();
        let mut out = Vec::new();
        for k in range.start..range.end.min(nnz) {
            // Column = last index j with jc[j] <= k.
            let col = match jcv.binary_search_by(|&v| {
                if v <= k {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Greater
                }
            }) {
                Ok(i) | Err(i) => i.saturating_sub(1),
            };
            let row = self
                .raw(ir, k)
                .map(|v| Self::num_to_f64(&v) as usize)
                .unwrap_or(0);
            let value = self.scalar_at(real, imag, k);
            out.push(SparseEntry { row, col, value });
        }
        Ok(out)
    }

    fn inner(&self) -> Result<Option<Box<dyn Node>>> {
        match &self.arr.body {
            Body::Function { inner } | Body::Opaque { inner, .. } => Ok(Some(self.child(*inner)?)),
            _ => Ok(None),
        }
    }
}

/// True if the file header identifies a v7.3 (HDF5-based) MAT-file.
pub fn is_v73(bytes: &[u8]) -> bool {
    bytes.len() >= 128 && String::from_utf8_lossy(&bytes[..116]).contains("MATLAB 7.3")
}
