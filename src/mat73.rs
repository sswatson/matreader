//! Lazy reader for MAT v7.3 files (HDF5-based), built on `hdf5-pure`.
//!
//! The file is opened in streaming mode so nothing is buffered up front.
//! Numeric data is fetched per chunk (or per contiguous byte run) straight
//! from the file, so scrolling around a multi-gigabyte matrix only touches the
//! chunks under the viewport.

use crate::model::{Class, Kind, MatFile, Node, Scalar, Shape, SparseEntry, VarInfo, Window};
use anyhow::{anyhow, bail, Context, Result};
use hdf5_pure::{
    AttrValue, Chunk, Dataset, Datatype, DatatypeByteOrder, File, FileAccessProperties, Group,
    Layout, MetadataCacheConfig, Object,
};
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};

/// Shared per-file state.
struct Ctx {
    raw: std::fs::File,
}

impl Ctx {
    /// Read bytes at an absolute file offset. (hdf5-pure reports chunk and
    /// contiguous-storage addresses with the superblock base already applied.)
    fn read_at(&self, address: u64, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        read_exact_at(&self.raw, address, &mut buf)?;
        Ok(buf)
    }
}

#[cfg(unix)]
fn read_exact_at(f: &std::fs::File, offset: u64, buf: &mut [u8]) -> Result<()> {
    use std::os::unix::fs::FileExt;
    f.read_exact_at(buf, offset)
        .with_context(|| format!("reading {} bytes at offset {}", buf.len(), offset))
}

#[cfg(windows)]
fn read_exact_at(f: &std::fs::File, offset: u64, buf: &mut [u8]) -> Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        let n = f.seek_read(&mut buf[done..], offset + done as u64)?;
        if n == 0 {
            bail!("short read at offset {}", offset);
        }
        done += n;
    }
    Ok(())
}

fn attr_str(attrs: &HashMap<String, AttrValue>, name: &str) -> Option<String> {
    match attrs.get(name)? {
        AttrValue::AsciiString(s) | AttrValue::String(s) => Some(s.clone()),
        AttrValue::AsciiStringSized { value, .. } | AttrValue::StringSized { value, .. } => {
            Some(value.clone())
        }
        AttrValue::AsciiStringArray(v) | AttrValue::StringArray(v) => v.first().cloned(),
        _ => None,
    }
}

fn attr_u64(attrs: &HashMap<String, AttrValue>, name: &str) -> Option<u64> {
    Some(match attrs.get(name)? {
        AttrValue::U8(v) => *v as u64,
        AttrValue::U16(v) => *v as u64,
        AttrValue::U32(v) => *v as u64,
        AttrValue::U64(v) => *v,
        AttrValue::I8(v) => *v as u64,
        AttrValue::I16(v) => *v as u64,
        AttrValue::I32(v) => *v as u64,
        AttrValue::I64(v) => *v as u64,
        AttrValue::F64(v) => *v as u64,
        AttrValue::F32(v) => *v as u64,
        AttrValue::U8Array(v) => *v.first()? as u64,
        AttrValue::U64Array(v) => *v.first()?,
        AttrValue::I32Array(v) => *v.first()? as u64,
        AttrValue::F64Array(v) => *v.first()? as u64,
        _ => return None,
    })
}

fn attr_str_list(attrs: &HashMap<String, AttrValue>, name: &str) -> Option<Vec<String>> {
    match attrs.get(name)? {
        AttrValue::VarLenAsciiCharArray(v) => Some(v.clone()),
        AttrValue::AsciiStringArray(v) | AttrValue::StringArray(v) => Some(v.clone()),
        AttrValue::AsciiString(s) | AttrValue::String(s) => Some(vec![s.clone()]),
        _ => None,
    }
}

/// How to decode one element of a dataset.
#[derive(Debug, Clone)]
enum Decode {
    Float {
        size: u32,
        big: bool,
    },
    Int {
        size: u32,
        signed: bool,
        big: bool,
    },
    Complex {
        real_off: usize,
        imag_off: usize,
        part: Box<Decode>,
    },
    Reference,
    Other,
}

fn decode_for(dt: &Datatype) -> Decode {
    match dt {
        Datatype::FloatingPoint {
            size, byte_order, ..
        } => Decode::Float {
            size: *size,
            big: matches!(byte_order, DatatypeByteOrder::BigEndian),
        },
        Datatype::FixedPoint {
            size,
            byte_order,
            signed,
            ..
        } => Decode::Int {
            size: *size,
            signed: *signed,
            big: matches!(byte_order, DatatypeByteOrder::BigEndian),
        },
        Datatype::Compound { members, .. } => {
            let real = members.iter().find(|m| m.name == "real");
            let imag = members.iter().find(|m| m.name == "imag");
            match (real, imag) {
                (Some(r), Some(i)) => Decode::Complex {
                    real_off: r.byte_offset as usize,
                    imag_off: i.byte_offset as usize,
                    part: Box::new(decode_for(&r.datatype)),
                },
                _ => Decode::Other,
            }
        }
        Datatype::Reference { .. } => Decode::Reference,
        Datatype::Enumeration { base_type, .. } => decode_for(base_type),
        _ => Decode::Other,
    }
}

fn decode_f64(bytes: &[u8], d: &Decode) -> f64 {
    match d {
        Decode::Float { size: 8, big } => {
            let a: [u8; 8] = bytes[..8].try_into().unwrap();
            if *big {
                f64::from_be_bytes(a)
            } else {
                f64::from_le_bytes(a)
            }
        }
        Decode::Float { size: 4, big } => {
            let a: [u8; 4] = bytes[..4].try_into().unwrap();
            (if *big {
                f32::from_be_bytes(a)
            } else {
                f32::from_le_bytes(a)
            }) as f64
        }
        Decode::Int { signed: true, .. } => decode_i64(bytes, d) as f64,
        Decode::Int { signed: false, .. } => decode_u64(bytes, d) as f64,
        _ => f64::NAN,
    }
}

fn decode_u64(bytes: &[u8], d: &Decode) -> u64 {
    if let Decode::Int { size, big, .. } = d {
        let n = (*size as usize).min(8).min(bytes.len());
        let mut a = [0u8; 8];
        if *big {
            a[8 - n..].copy_from_slice(&bytes[..n]);
            u64::from_be_bytes(a)
        } else {
            a[..n].copy_from_slice(&bytes[..n]);
            u64::from_le_bytes(a)
        }
    } else {
        decode_f64(bytes, d) as u64
    }
}

fn decode_i64(bytes: &[u8], d: &Decode) -> i64 {
    if let Decode::Int { size, .. } = d {
        let u = decode_u64(bytes, d);
        let bits = (*size).min(8) * 8;
        if bits >= 64 {
            u as i64
        } else {
            let shift = 64 - bits;
            ((u << shift) as i64) >> shift
        }
    } else {
        decode_f64(bytes, d) as i64
    }
}

/// Dense-dataset metadata needed for element reads.
struct Dense {
    ds: Dataset,
    /// HDF5 shape (row-major; reverse of MATLAB dims).
    h5_shape: Vec<u64>,
    elem_size: usize,
    decode: Decode,
    layout: Layout,
    filters: Vec<hdf5_pure::Filter>,
    /// Chunk index by chunk-origin coordinate (lazily listed).
    chunks: OnceLock<Result<HashMap<Vec<u64>, Chunk>, String>>,
    /// Decompressed chunk cache.
    chunk_cache: Mutex<ChunkCache>,
    /// Whole payload for compact datasets.
    compact: OnceLock<Result<Arc<Vec<u8>>, String>>,
}

struct ChunkCache {
    map: HashMap<Vec<u64>, Arc<Vec<u8>>>,
    order: std::collections::VecDeque<Vec<u64>>,
    bytes: usize,
}

const CHUNK_CACHE_BUDGET: usize = 256 * 1024 * 1024;

impl ChunkCache {
    fn new() -> Self {
        ChunkCache {
            map: HashMap::new(),
            order: Default::default(),
            bytes: 0,
        }
    }
    fn get(&self, k: &[u64]) -> Option<Arc<Vec<u8>>> {
        self.map.get(k).cloned()
    }
    fn put(&mut self, k: Vec<u64>, v: Arc<Vec<u8>>) {
        self.bytes += v.len();
        self.order.push_back(k.clone());
        self.map.insert(k, v);
        while self.bytes > CHUNK_CACHE_BUDGET && self.order.len() > 1 {
            if let Some(old) = self.order.pop_front() {
                if let Some(v) = self.map.remove(&old) {
                    self.bytes -= v.len();
                }
            }
        }
    }
}

impl Dense {
    fn new(ds: Dataset) -> Result<Dense> {
        let h5_shape = ds.shape()?;
        let dt = ds.datatype()?;
        let elem_size = ds.element_size()? as usize;
        let layout = ds.layout()?;
        let filters = ds.filter_pipeline();
        Ok(Dense {
            decode: decode_for(&dt),
            ds,
            h5_shape,
            elem_size,
            layout,
            filters,
            chunks: OnceLock::new(),
            chunk_cache: Mutex::new(ChunkCache::new()),
            compact: OnceLock::new(),
        })
    }

    fn chunk_map(&self) -> Result<&HashMap<Vec<u64>, Chunk>> {
        let r = self.chunks.get_or_init(|| {
            self.ds
                .chunks()
                .map(|v| v.into_iter().map(|c| (c.offset.clone(), c)).collect())
                .map_err(|e| e.to_string())
        });
        r.as_ref().map_err(|e| anyhow!("listing chunks: {}", e))
    }

    fn compact_bytes(&self) -> Result<Arc<Vec<u8>>> {
        let r = self
            .compact
            .get_or_init(|| self.ds.read_raw().map(Arc::new).map_err(|e| e.to_string()));
        r.clone().map_err(|e| anyhow!("reading dataset: {}", e))
    }

    fn expected_chunk_len(&self, chunk_shape: &[u64]) -> usize {
        chunk_shape.iter().product::<u64>() as usize * self.elem_size
    }

    fn load_chunk(
        &self,
        ctx: &Ctx,
        chunk_shape: &[u64],
        origin: &[u64],
    ) -> Result<Option<Arc<Vec<u8>>>> {
        if let Some(v) = self.chunk_cache.lock().unwrap().get(origin) {
            return Ok(Some(v));
        }
        let chunk = match self.chunk_map()?.get(origin) {
            Some(c) => c.clone(),
            None => return Ok(None), // never written: fill value
        };
        let raw = ctx.read_at(chunk.address, chunk.storage_size as usize)?;
        let expected = self.expected_chunk_len(chunk_shape);
        let data = apply_filters(
            raw,
            &self.filters,
            chunk.filter_mask,
            self.elem_size,
            expected,
        )?;
        let data = Arc::new(data);
        self.chunk_cache
            .lock()
            .unwrap()
            .put(origin.to_vec(), data.clone());
        Ok(Some(data))
    }

    /// Bytes of the element at MATLAB linear index `linear` (= HDF5 row-major
    /// index over the reversed dims).
    fn element_bytes(&self, ctx: &Ctx, h5_index: &[u64], linear: u64) -> Result<Vec<u8>> {
        let es = self.elem_size;
        match &self.layout {
            Layout::Compact { .. } => {
                let all = self.compact_bytes()?;
                let off = linear as usize * es;
                if off + es <= all.len() {
                    Ok(all[off..off + es].to_vec())
                } else {
                    Ok(vec![0; es])
                }
            }
            Layout::Contiguous {
                address: Some(addr),
                ..
            } => ctx.read_at(addr + linear * es as u64, es),
            Layout::Contiguous { address: None, .. } => Ok(vec![0; es]),
            Layout::Chunked { chunk_shape, .. } => {
                let mut origin = Vec::with_capacity(h5_index.len());
                let mut within = 0u64;
                let mut stride = 1u64;
                // Row-major strides over the chunk shape, innermost last.
                let mut strides = vec![1u64; chunk_shape.len()];
                for d in (0..chunk_shape.len()).rev() {
                    strides[d] = stride;
                    stride *= chunk_shape[d];
                }
                for (d, &i) in h5_index.iter().enumerate() {
                    let cs = chunk_shape[d].max(1);
                    let o = (i / cs) * cs;
                    origin.push(o);
                    within += (i - o) * strides[d];
                }
                match self.load_chunk(ctx, chunk_shape, &origin)? {
                    Some(data) => {
                        let off = within as usize * es;
                        if off + es <= data.len() {
                            Ok(data[off..off + es].to_vec())
                        } else {
                            Ok(vec![0; es])
                        }
                    }
                    None => Ok(vec![0; es]),
                }
            }
            _ => bail!("unsupported dataset layout"),
        }
    }
}

/// Undo an HDF5 filter pipeline on one chunk.
fn apply_filters(
    mut data: Vec<u8>,
    filters: &[hdf5_pure::Filter],
    filter_mask: u32,
    elem_size: usize,
    expected: usize,
) -> Result<Vec<u8>> {
    for (i, f) in filters.iter().enumerate().rev() {
        if filter_mask & (1 << i) != 0 {
            continue;
        }
        match f.id {
            1 => {
                // deflate (zlib stream)
                let mut out = Vec::with_capacity(expected);
                flate2::read::ZlibDecoder::new(&data[..])
                    .take(expected as u64 + 1)
                    .read_to_end(&mut out)
                    .context("inflating chunk")?;
                data = out;
            }
            2 => {
                // shuffle: bytes are grouped by byte position across elements
                let es = f.client_data.first().copied().unwrap_or(elem_size as u32) as usize;
                if es > 1 && data.len().is_multiple_of(es) {
                    let n = data.len() / es;
                    let mut out = vec![0u8; data.len()];
                    for j in 0..es {
                        for k in 0..n {
                            out[k * es + j] = data[j * n + k];
                        }
                    }
                    data = out;
                }
            }
            3 => {
                // fletcher32: checksum appended
                if data.len() >= 4 {
                    data.truncate(data.len() - 4);
                }
            }
            other => bail!(
                "unsupported HDF5 filter {} ({})",
                other,
                f.name.clone().unwrap_or_default()
            ),
        }
    }
    Ok(data)
}

enum Payload {
    /// Dense numeric/char/logical data.
    Dense(Dense),
    /// Empty array; dims already in `shape`.
    Empty,
    /// Cell array: dataset of object references.
    Cell {
        ds: Dataset,
        objects: OnceLock<Result<Arc<Vec<Object>>, String>>,
    },
    /// Scalar struct: group with one child per field.
    Struct { group: Group, fields: Vec<String> },
    /// Struct array: group whose fields are reference datasets.
    StructArray {
        group: Group,
        fields: Vec<String>,
        refs: Mutex<HashMap<usize, Arc<Vec<Object>>>>,
    },
    Sparse {
        jc: Vec<u64>,
        ir: Option<Dataset>,
        data: Option<Dense>,
    },
    /// MCOS object metadata (uint32 array) or other undecodable object.
    Opaque {
        meta: Option<Dense>,
        class_name: String,
    },
}

pub struct Mat73Node {
    ctx: Arc<Ctx>,
    class: Class,
    shape: Shape,
    complex: bool,
    payload: Payload,
    extra_info: Vec<(String, String)>,
}

fn reversed_dims(h5: &[u64]) -> Vec<usize> {
    h5.iter().rev().map(|&d| d as usize).collect()
}

impl Mat73Node {
    fn from_object(ctx: Arc<Ctx>, obj: &Object) -> Result<Mat73Node> {
        match obj {
            Object::Dataset(ds) => Self::from_dataset(ctx, (**ds).clone()),
            Object::Group(g) => Self::from_group(ctx, g.clone()),
            _ => bail!("unsupported object reference target"),
        }
    }

    fn from_dataset(ctx: Arc<Ctx>, ds: Dataset) -> Result<Mat73Node> {
        let attrs = ds.attrs().unwrap_or_default();
        let class_name = attr_str(&attrs, "MATLAB_class");
        let dt = ds.datatype()?;
        let h5_shape = ds.shape()?;
        let mut extra_info = Vec::new();
        if let Some(l) = attr_u64(&attrs, "MATLAB_int_decode") {
            extra_info.push(("MATLAB_int_decode".into(), l.to_string()));
        }
        extra_info.push(("HDF5 datatype".into(), format!("{}", dt)));
        match ds.layout()? {
            Layout::Chunked { chunk_shape, .. } => {
                let cs: Vec<String> = chunk_shape.iter().rev().map(|d| d.to_string()).collect();
                extra_info.push((
                    "layout".into(),
                    format!("chunked ({} per chunk)", cs.join("x")),
                ));
                let names: Vec<String> = ds
                    .filter_pipeline()
                    .iter()
                    .map(|f| f.name.clone().unwrap_or_else(|| format!("filter {}", f.id)))
                    .collect();
                if !names.is_empty() {
                    extra_info.push(("filters".into(), names.join(", ")));
                }
            }
            Layout::Contiguous { .. } => extra_info.push(("layout".into(), "contiguous".into())),
            Layout::Compact { .. } => extra_info.push(("layout".into(), "compact".into())),
            _ => {}
        }

        // Empty arrays store their MATLAB dims as the data.
        if attr_u64(&attrs, "MATLAB_empty") == Some(1) {
            let dims: Vec<usize> = ds.read_u64()?.iter().map(|&d| d as usize).collect();
            let class = class_name
                .as_deref()
                .map(Class::from_matlab_name)
                .unwrap_or(Class::Double);
            let shape = Shape::new(dims);
            let payload = match class {
                Class::Struct => {
                    let fields = attr_str_list(&attrs, "MATLAB_fields").unwrap_or_default();
                    Payload::StructArray {
                        group: ds_dummy_group()?,
                        fields,
                        refs: Mutex::new(HashMap::new()),
                    }
                }
                Class::Cell => Payload::Cell {
                    ds: ds.clone(),
                    objects: OnceLock::from(Ok(Arc::new(Vec::new()))),
                },
                _ => Payload::Empty,
            };
            return Ok(Mat73Node {
                ctx,
                class,
                shape,
                complex: false,
                payload,
                extra_info,
            });
        }

        let object_decode = attr_u64(&attrs, "MATLAB_object_decode");
        let dims = reversed_dims(&h5_shape);

        if matches!(dt, Datatype::Reference { .. }) {
            // Cell array (or, without a class, a struct-array field; callers
            // handle that case before reaching here).
            return Ok(Mat73Node {
                ctx,
                class: Class::Cell,
                shape: Shape::new(dims),
                complex: false,
                payload: Payload::Cell {
                    ds,
                    objects: OnceLock::new(),
                },
                extra_info,
            });
        }

        let class = match &class_name {
            Some(n) => Class::from_matlab_name(n),
            None => class_from_datatype(&dt),
        };

        if object_decode == Some(3) || matches!(class, Class::Opaque(_)) {
            let name = class.name();
            let dense = Dense::new(ds)?;
            // Metadata layout: [0xDD000000, ndims, dims..., object ids..., class id]
            let mut shape = Shape::new(vec![1, 1]);
            if let Ok(words) = dense.ds.read_u32() {
                if words.len() >= 2 && words[0] == 0xDD00_0000 {
                    let nd = words[1] as usize;
                    if nd >= 1 && words.len() >= 2 + nd {
                        shape = Shape::new(words[2..2 + nd].iter().map(|&d| d as usize).collect());
                    }
                }
            }
            return Ok(Mat73Node {
                ctx,
                class: Class::Opaque(name.clone()),
                shape,
                complex: false,
                payload: Payload::Opaque {
                    meta: Some(dense),
                    class_name: name,
                },
                extra_info,
            });
        }

        let complex = matches!(decode_for(&dt), Decode::Complex { .. });
        let dense = Dense::new(ds)?;
        Ok(Mat73Node {
            ctx,
            class,
            shape: Shape::new(dims),
            complex,
            payload: Payload::Dense(dense),
            extra_info,
        })
    }

    fn from_group(ctx: Arc<Ctx>, group: Group) -> Result<Mat73Node> {
        let attrs = group.attrs().unwrap_or_default();
        let class_name = attr_str(&attrs, "MATLAB_class");
        let mut extra_info = Vec::new();

        if let Some(nrows) = attr_u64(&attrs, "MATLAB_sparse") {
            let class = class_name
                .as_deref()
                .map(Class::from_matlab_name)
                .unwrap_or(Class::Double);
            let jc = match group.dataset("jc") {
                Ok(d) => d.read_u64()?,
                Err(_) => vec![0],
            };
            let ncols = jc.len().saturating_sub(1);
            let ir = group.dataset("ir").ok();
            let data = match group.dataset("data") {
                Ok(d) => Some(Dense::new(d)?),
                Err(_) => None,
            };
            let complex = data
                .as_ref()
                .map(|d| matches!(d.decode, Decode::Complex { .. }))
                .unwrap_or(false);
            extra_info.push((
                "storage".into(),
                "compressed sparse column (HDF5 group)".into(),
            ));
            return Ok(Mat73Node {
                ctx,
                class,
                shape: Shape::new(vec![nrows as usize, ncols]),
                complex,
                payload: Payload::Sparse { jc, ir, data },
                extra_info,
            });
        }

        let fields = attr_str_list(&attrs, "MATLAB_fields").unwrap_or_else(|| {
            let mut names = group.datasets().unwrap_or_default();
            names.extend(group.groups().unwrap_or_default());
            names
        });

        let class = match class_name.as_deref() {
            Some("struct") | None => Class::Struct,
            Some("function_handle") => Class::FunctionHandle,
            Some(other) => Class::from_matlab_name(other),
        };

        // A struct array stores each field as a reference dataset without a
        // MATLAB_class attribute, shaped like the struct array itself.
        if class == Class::Struct {
            if let Some(first) = fields.first() {
                if let Ok(ds) = group.dataset(first) {
                    let a = ds.attrs().unwrap_or_default();
                    if !a.contains_key("MATLAB_class")
                        && matches!(ds.datatype()?, Datatype::Reference { .. })
                    {
                        let dims = reversed_dims(&ds.shape()?);
                        return Ok(Mat73Node {
                            ctx,
                            class,
                            shape: Shape::new(dims),
                            complex: false,
                            payload: Payload::StructArray {
                                group,
                                fields,
                                refs: Mutex::new(HashMap::new()),
                            },
                            extra_info,
                        });
                    }
                }
            }
        }

        if let Class::Opaque(name) = &class {
            let name = name.clone();
            return Ok(Mat73Node {
                ctx,
                class: Class::Opaque(name.clone()),
                shape: Shape::new(vec![1, 1]),
                complex: false,
                payload: Payload::Struct { group, fields },
                extra_info: vec![(
                    "note".into(),
                    format!("object of class {} stored as a group", name),
                )],
            });
        }

        Ok(Mat73Node {
            ctx,
            class,
            shape: Shape::new(vec![1, 1]),
            complex: false,
            payload: Payload::Struct { group, fields },
            extra_info,
        })
    }

    fn child_by_name(&self, group: &Group, name: &str) -> Result<Box<dyn Node>> {
        if let Ok(ds) = group.dataset(name) {
            return Ok(Box::new(Self::from_dataset(self.ctx.clone(), ds)?));
        }
        if let Ok(g) = group.group(name) {
            return Ok(Box::new(Self::from_group(self.ctx.clone(), g)?));
        }
        bail!("field '{}' not found", name)
    }

    fn cell_objects(&self) -> Result<Arc<Vec<Object>>> {
        match &self.payload {
            Payload::Cell { ds, objects } => {
                let r = objects
                    .get_or_init(|| ds.dereference().map(Arc::new).map_err(|e| e.to_string()));
                r.clone()
                    .map_err(|e| anyhow!("resolving cell references: {}", e))
            }
            _ => bail!("not a cell array"),
        }
    }

    fn struct_field_refs(&self, field: usize) -> Result<Arc<Vec<Object>>> {
        match &self.payload {
            Payload::StructArray {
                group,
                fields,
                refs,
            } => {
                if let Some(v) = refs.lock().unwrap().get(&field) {
                    return Ok(v.clone());
                }
                let name = fields
                    .get(field)
                    .ok_or_else(|| anyhow!("no field {}", field))?;
                let ds = group.dataset(name)?;
                let objs = Arc::new(
                    ds.dereference()
                        .map_err(|e| anyhow!("resolving field references: {}", e))?,
                );
                refs.lock().unwrap().insert(field, objs.clone());
                Ok(objs)
            }
            _ => bail!("not a struct array"),
        }
    }

    fn dense(&self) -> Option<&Dense> {
        match &self.payload {
            Payload::Dense(d) => Some(d),
            Payload::Opaque { meta: Some(d), .. } => Some(d),
            _ => None,
        }
    }

    fn scalar_from_bytes(&self, bytes: &[u8], decode: &Decode, class: &Class) -> Scalar {
        match decode {
            Decode::Complex {
                real_off,
                imag_off,
                part,
            } => {
                let re = decode_f64(&bytes[*real_off..], part);
                let im = decode_f64(&bytes[*imag_off..], part);
                Scalar::Complex(re, im)
            }
            Decode::Float { .. } => match class {
                Class::Logical => Scalar::Bool(decode_f64(bytes, decode) != 0.0),
                _ => Scalar::Float(decode_f64(bytes, decode)),
            },
            Decode::Int { signed, .. } => match class {
                Class::Logical => Scalar::Bool(decode_u64(bytes, decode) != 0),
                Class::Char => Scalar::Char(decode_u64(bytes, decode) as u32),
                Class::Double | Class::Single => Scalar::Float(decode_f64(bytes, decode)),
                _ => {
                    if *signed {
                        Scalar::Int(decode_i64(bytes, decode))
                    } else {
                        Scalar::Uint(decode_u64(bytes, decode))
                    }
                }
            },
            _ => Scalar::Float(f64::NAN),
        }
    }
}

fn class_from_datatype(dt: &Datatype) -> Class {
    match dt {
        Datatype::FloatingPoint { size: 8, .. } => Class::Double,
        Datatype::FloatingPoint { size: 4, .. } => Class::Single,
        Datatype::FixedPoint { size, signed, .. } => match (size, signed) {
            (1, true) => Class::Int8,
            (1, false) => Class::Uint8,
            (2, true) => Class::Int16,
            (2, false) => Class::Uint16,
            (4, true) => Class::Int32,
            (4, false) => Class::Uint32,
            (8, true) => Class::Int64,
            (8, false) => Class::Uint64,
            _ => Class::Unknown(format!("{}", dt)),
        },
        Datatype::Compound { members, .. } => members
            .iter()
            .find(|m| m.name == "real")
            .map(|m| class_from_datatype(&m.datatype))
            .unwrap_or(Class::Unknown("compound".into())),
        other => Class::Unknown(format!("{}", other)),
    }
}

// An empty struct array has no group; use a placeholder that is never touched
// because it has zero elements (or zero fields).
fn ds_dummy_group() -> Result<Group> {
    // hdf5-pure has no way to construct a detached group, so we open a tiny
    // in-memory file once and reuse its root.
    static DUMMY: OnceLock<Result<File, String>> = OnceLock::new();
    let f = DUMMY
        .get_or_init(|| {
            let bytes = hdf5_pure::FileBuilder::new()
                .finish()
                .map_err(|e| e.to_string())?;
            File::from_bytes(bytes).map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| anyhow!("{}", e))?;
    Ok(f.root())
}

impl Node for Mat73Node {
    fn class(&self) -> Class {
        self.class.clone()
    }

    fn shape(&self) -> Shape {
        self.shape.clone()
    }

    fn is_complex(&self) -> bool {
        self.complex
    }

    fn is_sparse(&self) -> bool {
        matches!(self.payload, Payload::Sparse { .. })
    }

    fn kind(&self) -> Kind {
        match &self.payload {
            Payload::Dense(_) | Payload::Empty => {
                if self.class == Class::Char {
                    Kind::Char
                } else {
                    Kind::Numeric
                }
            }
            Payload::Cell { .. } => Kind::Cell,
            Payload::Struct { .. } | Payload::StructArray { .. } => {
                if self.class == Class::FunctionHandle {
                    Kind::Function
                } else {
                    Kind::Struct
                }
            }
            Payload::Sparse { .. } => Kind::Sparse,
            Payload::Opaque { .. } => Kind::Opaque,
        }
    }

    fn info(&self) -> Vec<(String, String)> {
        self.extra_info.clone()
    }

    fn description(&self) -> Option<String> {
        match &self.payload {
            Payload::Opaque { class_name, .. } => Some(format!(
                "MCOS object of class '{}'. Its contents are stored in the file's #subsystem# \
                 group, which matreader does not decode; only the object metadata is shown.",
                class_name
            )),
            _ => None,
        }
    }

    fn field_names(&self) -> Result<Vec<String>> {
        match &self.payload {
            Payload::Struct { fields, .. } | Payload::StructArray { fields, .. } => {
                Ok(fields.clone())
            }
            _ => Ok(Vec::new()),
        }
    }

    fn field(&self, elem: usize, field: usize) -> Result<Box<dyn Node>> {
        match &self.payload {
            Payload::Struct { group, fields } => {
                let name = fields
                    .get(field)
                    .ok_or_else(|| anyhow!("no field {}", field))?;
                self.child_by_name(group, name)
            }
            Payload::StructArray { .. } => {
                let objs = self.struct_field_refs(field)?;
                let obj = objs
                    .get(elem)
                    .ok_or_else(|| anyhow!("struct element {} out of range", elem))?;
                Ok(Box::new(Self::from_object(self.ctx.clone(), obj)?))
            }
            _ => bail!("not a struct"),
        }
    }

    fn cell(&self, index: usize) -> Result<Box<dyn Node>> {
        let objs = self.cell_objects()?;
        let obj = objs
            .get(index)
            .ok_or_else(|| anyhow!("cell index {} out of range", index))?;
        Ok(Box::new(Self::from_object(self.ctx.clone(), obj)?))
    }

    fn prepare(&self) -> Result<()> {
        match &self.payload {
            Payload::Cell { .. } => {
                self.cell_objects()?;
            }
            Payload::StructArray { fields, .. } => {
                for f in 0..fields.len() {
                    self.struct_field_refs(f)?;
                }
            }
            Payload::Dense(d) => {
                if matches!(d.layout, Layout::Chunked { .. }) {
                    d.chunk_map()?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn read_window(&self, window: &Window) -> Result<Vec<Scalar>> {
        let dense = match &self.payload {
            Payload::Empty => return Ok(Vec::new()),
            _ => self.dense().ok_or_else(|| anyhow!("not a dense array"))?,
        };
        let class = match &self.payload {
            Payload::Opaque { .. } => Class::Uint32,
            _ => self.class.clone(),
        };
        let shape = &self.shape;
        let mut out = Vec::with_capacity(window.numel());
        let nd = dense.h5_shape.len();
        for subs in window.subscripts() {
            let linear = shape.linear_index(&subs) as u64;
            // HDF5 index is the reversed MATLAB subscript vector.
            let mut h5_index: Vec<u64> = subs.iter().rev().map(|&s| s as u64).collect();
            if h5_index.len() < nd {
                let mut padded = vec![0u64; nd - h5_index.len()];
                padded.extend(h5_index);
                h5_index = padded;
            } else if h5_index.len() > nd {
                h5_index = h5_index[h5_index.len() - nd..].to_vec();
            }
            let bytes = dense.element_bytes(&self.ctx, &h5_index, linear)?;
            out.push(self.scalar_from_bytes(&bytes, &dense.decode, &class));
        }
        Ok(out)
    }

    fn nnz(&self) -> usize {
        match &self.payload {
            Payload::Sparse { jc, .. } => jc.last().copied().unwrap_or(0) as usize,
            _ => 0,
        }
    }

    fn sparse_entries(&self, range: std::ops::Range<usize>) -> Result<Vec<SparseEntry>> {
        let (jc, ir, data) = match &self.payload {
            Payload::Sparse { jc, ir, data } => (jc, ir, data),
            _ => bail!("not a sparse matrix"),
        };
        let nnz = self.nnz();
        let start = range.start.min(nnz);
        let end = range.end.min(nnz);
        if start >= end {
            return Ok(Vec::new());
        }
        let count = end - start;
        let rows: Vec<u64> = match ir {
            Some(ds) => ds.read_u64_rows(start as u64, count as u64)?,
            None => vec![0; count],
        };
        let mut out = Vec::with_capacity(count);
        for (i, k) in (start..end).enumerate() {
            let col = match jc.binary_search_by(|&v| {
                if v as usize <= k {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Greater
                }
            }) {
                Ok(i) | Err(i) => i.saturating_sub(1),
            };
            let value = match data {
                Some(d) => {
                    let bytes = d.element_bytes(&self.ctx, &[k as u64], k as u64)?;
                    self.scalar_from_bytes(&bytes, &d.decode, &self.class)
                }
                None => Scalar::Float(0.0),
            };
            out.push(SparseEntry {
                row: rows.get(i).copied().unwrap_or(0) as usize,
                col,
                value,
            });
        }
        Ok(out)
    }

    fn inner(&self) -> Result<Option<Box<dyn Node>>> {
        match &self.payload {
            Payload::Opaque { meta: Some(d), .. } => {
                let ds = d.ds.clone();
                let dense = Dense::new(ds)?;
                let shape = Shape::new(reversed_dims(&dense.h5_shape));
                Ok(Some(Box::new(Mat73Node {
                    ctx: self.ctx.clone(),
                    class: Class::Uint32,
                    shape,
                    complex: false,
                    payload: Payload::Dense(dense),
                    extra_info: vec![("note".into(), "MCOS object metadata".into())],
                })))
            }
            _ => Ok(None),
        }
    }
}

pub struct Mat73File {
    _file: File,
    ctx: Arc<Ctx>,
    header_text: String,
    vars: Vec<VarInfo>,
    members: Vec<Object>,
}

impl Mat73File {
    pub fn open(path: &Path) -> Result<Mat73File> {
        let mut raw =
            std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut head = [0u8; 128];
        raw.read_exact(&mut head)?;
        let header_text = String::from_utf8_lossy(&head[..116])
            .trim_end_matches(['\0', ' '])
            .to_string();

        let access = FileAccessProperties::new().with_metadata_cache(
            MetadataCacheConfig::new(64 * 1024 * 1024).with_max_entry_bytes(4 * 1024 * 1024),
        );
        let file = File::open_streaming_with_options(path, access)
            .map_err(|e| anyhow!("opening HDF5 container: {}", e))?;
        let ctx = Arc::new(Ctx { raw });

        let root = file.root();
        let mut members: Vec<(String, Object)> = Vec::new();
        for (name, ds) in root.iter_datasets()? {
            if !name.starts_with('#') {
                members.push((name, Object::Dataset(Box::new(ds))));
            }
        }
        for (name, g) in root.iter_groups()? {
            if !name.starts_with('#') {
                members.push((name, Object::Group(g)));
            }
        }
        members.sort_by(|a, b| a.0.cmp(&b.0));

        let mut vars = Vec::new();
        let mut objs = Vec::new();
        for (name, obj) in members {
            let info = match Mat73Node::from_object(ctx.clone(), &obj) {
                Ok(node) => VarInfo {
                    name: name.clone(),
                    class: node.class(),
                    shape: node.shape(),
                    is_complex: node.is_complex(),
                    is_sparse: node.is_sparse(),
                    is_global: false,
                    storage_bytes: storage_bytes(&obj),
                    compressed: is_compressed(&obj),
                },
                Err(e) => VarInfo {
                    name: format!("{} <unreadable: {}>", name, e),
                    class: Class::Unknown("?".into()),
                    shape: Shape::new(vec![0, 0]),
                    is_complex: false,
                    is_sparse: false,
                    is_global: false,
                    storage_bytes: 0,
                    compressed: false,
                },
            };
            vars.push(info);
            objs.push(obj);
        }

        Ok(Mat73File {
            _file: file,
            ctx,
            header_text,
            vars,
            members: objs,
        })
    }
}

fn storage_bytes(obj: &Object) -> u64 {
    match obj {
        Object::Dataset(ds) => match ds.layout() {
            Ok(Layout::Chunked { .. }) => ds
                .chunks()
                .map(|cs| cs.iter().map(|c| c.storage_size).sum())
                .unwrap_or(0),
            Ok(Layout::Contiguous { size, .. }) | Ok(Layout::Compact { size }) => size,
            _ => 0,
        },
        Object::Group(_) => 0,
        _ => 0,
    }
}

fn is_compressed(obj: &Object) -> bool {
    match obj {
        Object::Dataset(ds) => !ds.filters().is_empty(),
        _ => false,
    }
}

impl MatFile for Mat73File {
    fn format(&self) -> String {
        "MAT v7.3 (HDF5)".into()
    }

    fn header_text(&self) -> String {
        self.header_text.clone()
    }

    fn variables(&self) -> &[VarInfo] {
        &self.vars
    }

    fn load(&self, index: usize, _cancel: &AtomicBool) -> Result<Box<dyn Node>> {
        let obj = self
            .members
            .get(index)
            .ok_or_else(|| anyhow!("no variable at index {}", index))?;
        let node = Mat73Node::from_object(self.ctx.clone(), obj)?;
        node.prepare()?;
        Ok(Box::new(node))
    }
}
