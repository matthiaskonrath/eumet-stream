//! A small, pure-Rust reader for the subset of HDF5 used by netCDF-4 files.
//!
//! EUMETCast products (NWC SAF, LSA SAF, EPS, ...) are netCDF-4, which is HDF5
//! underneath. The usual Rust bindings wrap the HDF5 C library, which needs a C
//! toolchain and CMake; neither is available here, so this module parses the
//! container directly.
//!
//! Only what these files actually use is implemented: superblock v0/v2/v3,
//! object headers v1 and v2, compact and dense (fractal-heap) link and
//! attribute storage, contiguous and chunked data layouts, and the deflate,
//! shuffle and fletcher32 filters.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

pub mod btree;
pub mod dtype;
pub mod filter;
pub mod heap;

pub use dtype::{Datatype, DatatypeClass};

pub type Result<T> = std::result::Result<T, Error>;

/// Largest array this reader will materialise, as a guard on shapes read from
/// the file. A full SEVIRI disc is 3712 squared at two bytes, about 27 MB.
pub const MAX_DATASET_BYTES: usize = 512 * 1024 * 1024;

#[derive(Debug)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "hdf5: {}", self.0)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error(e.to_string())
    }
}

pub(crate) fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error(msg.into()))
}

/// An address that is "not present" is encoded as all bits set.
pub(crate) fn is_undef(addr: u64, size: u8) -> bool {
    if size >= 8 {
        addr == u64::MAX
    } else {
        addr == (1u64 << (size as u32 * 8)) - 1
    }
}

// ---------------------------------------------------------------------------
// Little-endian cursor
// ---------------------------------------------------------------------------

/// A bounds-checked little-endian reader over an in-memory slice.
#[derive(Clone)]
pub struct Cur<'a> {
    pub d: &'a [u8],
    pub p: usize,
}

impl<'a> Cur<'a> {
    pub fn new(d: &'a [u8], p: usize) -> Self {
        Cur { d, p }
    }

    fn need(&self, n: usize) -> Result<()> {
        if self.p + n > self.d.len() {
            return err(format!(
                "read past end of file (offset {}, need {}, len {})",
                self.p,
                n,
                self.d.len()
            ));
        }
        Ok(())
    }

    pub fn u8(&mut self) -> Result<u8> {
        self.need(1)?;
        let v = self.d[self.p];
        self.p += 1;
        Ok(v)
    }

    pub fn u16(&mut self) -> Result<u16> {
        self.need(2)?;
        let v = u16::from_le_bytes([self.d[self.p], self.d[self.p + 1]]);
        self.p += 2;
        Ok(v)
    }

    pub fn u32(&mut self) -> Result<u32> {
        self.need(4)?;
        let mut b = [0u8; 4];
        b.copy_from_slice(&self.d[self.p..self.p + 4]);
        self.p += 4;
        Ok(u32::from_le_bytes(b))
    }

    pub fn u64(&mut self) -> Result<u64> {
        self.need(8)?;
        let mut b = [0u8; 8];
        b.copy_from_slice(&self.d[self.p..self.p + 8]);
        self.p += 8;
        Ok(u64::from_le_bytes(b))
    }

    /// Read an `n`-byte little-endian unsigned integer (n <= 8).
    pub fn var(&mut self, n: usize) -> Result<u64> {
        if n == 0 {
            return Ok(0);
        }
        if n > 8 {
            return err(format!("variable-width integer too wide: {n}"));
        }
        self.need(n)?;
        let mut v = 0u64;
        for i in 0..n {
            v |= (self.d[self.p + i] as u64) << (8 * i);
        }
        self.p += n;
        Ok(v)
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        self.need(n)?;
        let s = &self.d[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }

    /// Advance without reading, never past the end of the buffer.
    ///
    /// Some skips are sized from the file - array element counts, padding to an
    /// alignment - so an absurd value must not push the position somewhere the
    /// next bounds check cannot represent. Saturating here keeps `need` a plain
    /// comparison that cannot overflow, and the next read fails cleanly.
    pub fn skip(&mut self, n: usize) {
        self.p = self.p.saturating_add(n).min(self.d.len());
    }

    pub fn tag(&mut self, sig: &[u8; 4]) -> Result<()> {
        let got = self.bytes(4)?;
        if got != sig {
            return err(format!(
                "expected signature {:?}, found {:?}",
                std::str::from_utf8(sig).unwrap_or("?"),
                String::from_utf8_lossy(got)
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Object header messages
// ---------------------------------------------------------------------------

pub const MSG_DATASPACE: u16 = 0x0001;
pub const MSG_LINK_INFO: u16 = 0x0002;
pub const MSG_DATATYPE: u16 = 0x0003;
pub const MSG_FILL: u16 = 0x0005;
pub const MSG_LINK: u16 = 0x0006;
pub const MSG_LAYOUT: u16 = 0x0008;
pub const MSG_FILTER: u16 = 0x000B;
pub const MSG_ATTRIBUTE: u16 = 0x000C;
pub const MSG_CONTINUATION: u16 = 0x0010;
pub const MSG_SYMBOL_TABLE: u16 = 0x0011;
pub const MSG_ATTR_INFO: u16 = 0x0015;

/// A message located inside an object header, kept as a span into the file so
/// that borrows stay simple.
#[derive(Debug, Clone, Copy)]
pub struct RawMsg {
    pub typ: u16,
    pub off: usize,
    pub len: usize,
}

/// How a dataset's raw bytes are stored.
#[derive(Debug, Clone)]
pub enum Layout {
    Compact {
        off: usize,
        len: usize,
    },
    Contiguous {
        addr: u64,
        size: u64,
    },
    /// `dims` are the chunk edge lengths; `elem` the element size in bytes.
    Chunked {
        addr: u64,
        dims: Vec<u32>,
        elem: u32,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct FilterDef {
    pub id: u16,
    pub optional: bool,
}

/// A decoded attribute value.
#[derive(Debug, Clone)]
pub enum AttrValue {
    Text(String),
    Ints(Vec<i64>),
    Floats(Vec<f64>),
    Raw(Vec<u8>),
}

impl AttrValue {
    pub fn as_text(&self) -> Option<&str> {
        match self {
            AttrValue::Text(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Best-effort numeric read, so callers do not care whether a value was
    /// written as an integer or a float.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            AttrValue::Floats(v) => v.first().copied(),
            AttrValue::Ints(v) => v.first().map(|&x| x as f64),
            AttrValue::Text(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    pub fn as_f64_vec(&self) -> Vec<f64> {
        match self {
            AttrValue::Floats(v) => v.clone(),
            AttrValue::Ints(v) => v.iter().map(|&x| x as f64).collect(),
            _ => Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// File
// ---------------------------------------------------------------------------

pub struct H5File {
    pub data: Vec<u8>,
    pub off_size: u8,
    pub len_size: u8,
    pub root_addr: u64,
}

impl H5File {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let data = std::fs::read(path)?;
        Self::from_bytes(data)
    }

    pub fn from_bytes(data: Vec<u8>) -> Result<Self> {
        if data.len() < 16 || &data[0..8] != b"\x89HDF\r\n\x1a\n" {
            return err("not an HDF5 file (bad signature)");
        }
        let version = data[8];
        let (off_size, len_size, root_addr) = match version {
            0 | 1 => {
                let off_size = data[13];
                let len_size = data[14];
                // v0/v1: root group symbol table entry follows the driver info
                // address; its object header address is the second field.
                let mut c = Cur::new(&data, 56 + off_size as usize);
                let root = c.var(off_size as usize)?;
                (off_size, len_size, root)
            }
            2 | 3 => {
                let off_size = data[9];
                let len_size = data[10];
                let mut c = Cur::new(&data, 12);
                let _base = c.var(off_size as usize)?;
                let _ext = c.var(off_size as usize)?;
                let _eof = c.var(off_size as usize)?;
                let root = c.var(off_size as usize)?;
                (off_size, len_size, root)
            }
            v => return err(format!("unsupported superblock version {v}")),
        };

        Ok(H5File {
            data,
            off_size,
            len_size,
            root_addr,
        })
    }

    pub(crate) fn offset_at(&self, c: &mut Cur) -> Result<u64> {
        c.var(self.off_size as usize)
    }

    pub(crate) fn length_at(&self, c: &mut Cur) -> Result<u64> {
        c.var(self.len_size as usize)
    }

    /// Collect every message of an object header, following continuation blocks.
    pub fn object_messages(&self, addr: u64) -> Result<Vec<RawMsg>> {
        let mut out = Vec::new();
        let a = addr as usize;
        if a + 4 > self.data.len() {
            return err(format!("object header address {addr} out of range"));
        }

        if &self.data[a..a + 4] == b"OHDR" {
            let mut c = Cur::new(&self.data, a + 4);
            let ver = c.u8()?;
            if ver != 2 {
                return err(format!("unsupported OHDR version {ver}"));
            }
            let flags = c.u8()?;
            if flags & 0x20 != 0 {
                c.skip(16); // access/modification/change/birth times
            }
            if flags & 0x10 != 0 {
                c.skip(4); // attribute phase-change limits
            }
            let size_width = 1usize << (flags & 0x03);
            let chunk0 = c.var(size_width)? as usize;
            let order = flags & 0x04 != 0;
            self.walk_v2(c.p, c.p + chunk0, order, &mut out)?;
        } else {
            let mut c = Cur::new(&self.data, a);
            let ver = c.u8()?;
            if ver != 1 {
                return err(format!("unrecognised object header version {ver}"));
            }
            c.skip(1);
            let nmsgs = c.u16()? as usize;
            let _refcount = c.u32()?;
            let hdr_size = c.u32()? as usize;
            c.skip(4); // pad to 8-byte boundary
            self.walk_v1(c.p, c.p + hdr_size, nmsgs, &mut out)?;
        }
        Ok(out)
    }

    /// Version 2 headers pack messages back to back and may end with a gap too
    /// small to hold another message header.
    fn walk_v2(&self, start: usize, end: usize, order: bool, out: &mut Vec<RawMsg>) -> Result<()> {
        let hdr = if order { 6 } else { 4 };
        let mut p = start;
        let mut conts: Vec<(u64, u64)> = Vec::new();

        while p + hdr <= end {
            let mut c = Cur::new(&self.data, p);
            let typ = c.u8()? as u16;
            let size = c.u16()? as usize;
            let _flags = c.u8()?;
            if order {
                c.skip(2);
            }
            let doff = c.p;
            if doff + size > end {
                break; // trailing gap, not a real message
            }
            if typ == MSG_CONTINUATION {
                let mut cc = Cur::new(&self.data, doff);
                let off = self.offset_at(&mut cc)?;
                let len = self.length_at(&mut cc)?;
                conts.push((off, len));
            } else {
                out.push(RawMsg {
                    typ,
                    off: doff,
                    len: size,
                });
            }
            p = doff + size;
        }

        for (off, len) in conts {
            let a = off as usize;
            let mut c = Cur::new(&self.data, a);
            c.tag(b"OCHK")?;
            // The block length covers signature and trailing checksum.
            let body_end = a + len as usize - 4;
            self.walk_v2(c.p, body_end, order, out)?;
        }
        Ok(())
    }

    /// Version 1 headers align every message to an 8-byte boundary.
    fn walk_v1(&self, start: usize, end: usize, nmsgs: usize, out: &mut Vec<RawMsg>) -> Result<()> {
        let mut p = start;
        let mut seen = 0usize;
        let mut conts: Vec<(u64, u64)> = Vec::new();

        while p + 8 <= end && seen < nmsgs {
            let mut c = Cur::new(&self.data, p);
            let typ = c.u16()?;
            let size = c.u16()? as usize;
            let _flags = c.u8()?;
            c.skip(3);
            let doff = c.p;
            if doff + size > end {
                break;
            }
            if typ == MSG_CONTINUATION {
                let mut cc = Cur::new(&self.data, doff);
                let off = self.offset_at(&mut cc)?;
                let len = self.length_at(&mut cc)?;
                conts.push((off, len));
            } else {
                out.push(RawMsg {
                    typ,
                    off: doff,
                    len: size,
                });
            }
            p = doff + size;
            seen += 1;
        }

        for (off, len) in conts {
            let a = off as usize;
            self.walk_v1(a, a + len as usize, nmsgs - seen, out)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Groups and attributes
// ---------------------------------------------------------------------------

impl H5File {
    /// Member names and object-header addresses of a group.
    pub fn links(&self, group_addr: u64) -> Result<Vec<(String, u64)>> {
        let msgs = self.object_messages(group_addr)?;
        let mut out = Vec::new();

        // Compact storage keeps one Link message per member.
        for m in msgs.iter().filter(|m| m.typ == MSG_LINK) {
            let mut c = Cur::new(&self.data, m.off);
            if let Some(l) = self.parse_link(&mut c)? {
                out.push(l);
            }
        }
        if !out.is_empty() {
            return Ok(out);
        }

        // Dense storage puts the links in a fractal heap.
        for m in msgs.iter().filter(|m| m.typ == MSG_LINK_INFO) {
            let mut c = Cur::new(&self.data, m.off);
            let _ver = c.u8()?;
            let flags = c.u8()?;
            if flags & 0x01 != 0 {
                c.skip(8); // maximum creation index
            }
            let heap_addr = self.offset_at(&mut c)?;
            if is_undef(heap_addr, self.off_size) {
                continue;
            }
            let heap = heap::FractalHeap::open(self, heap_addr)?;
            for (start, end) in heap.object_areas(self)? {
                let mut c = Cur::new(&self.data, start);
                while c.p < end {
                    match self.parse_link(&mut c) {
                        Ok(Some(l)) => out.push(l),
                        // Free space at the tail of the block: stop here.
                        _ => break,
                    }
                }
            }
        }
        Ok(out)
    }

    fn parse_link(&self, c: &mut Cur) -> Result<Option<(String, u64)>> {
        let ver = c.u8()?;
        if ver != 1 {
            return Ok(None);
        }
        let flags = c.u8()?;
        let name_len_size = 1usize << (flags & 0x03);
        let link_type = if flags & 0x08 != 0 { c.u8()? } else { 0 };
        if flags & 0x04 != 0 {
            c.skip(8); // creation order
        }
        if flags & 0x10 != 0 {
            c.skip(1); // name character set
        }
        let nlen = c.var(name_len_size)? as usize;
        if nlen == 0 || nlen > 4096 {
            return Ok(None);
        }
        let name = String::from_utf8_lossy(c.bytes(nlen)?).to_string();
        match link_type {
            0 => Ok(Some((name, self.offset_at(c)?))),
            _ => Ok(None),
        }
    }

    /// All attributes attached to an object.
    pub fn attributes(&self, addr: u64) -> Result<HashMap<String, AttrValue>> {
        let msgs = self.object_messages(addr)?;
        let mut out = HashMap::new();

        for m in msgs.iter().filter(|m| m.typ == MSG_ATTRIBUTE) {
            let mut c = Cur::new(&self.data, m.off);
            if let Ok(Some((k, v))) = self.parse_attribute(&mut c) {
                out.insert(k, v);
            }
        }

        for m in msgs.iter().filter(|m| m.typ == MSG_ATTR_INFO) {
            let mut c = Cur::new(&self.data, m.off);
            let _ver = c.u8()?;
            let flags = c.u8()?;
            if flags & 0x01 != 0 {
                c.skip(2); // maximum creation index
            }
            let heap_addr = self.offset_at(&mut c)?;
            if is_undef(heap_addr, self.off_size) {
                continue;
            }
            let heap = heap::FractalHeap::open(self, heap_addr)?;
            for (start, end) in heap.object_areas(self)? {
                let mut c = Cur::new(&self.data, start);
                while c.p < end {
                    match self.parse_attribute(&mut c) {
                        Ok(Some((k, v))) => {
                            out.insert(k, v);
                        }
                        _ => break,
                    }
                }
            }
        }
        Ok(out)
    }

    /// Diagnostic walk of an object's dense attribute heap.
    pub fn attr_trace(&self, addr: u64) -> Result<Vec<String>> {
        fn hex(d: &[u8], p: usize, n: usize) -> String {
            d[p..(p + n).min(d.len())]
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(" ")
        }

        let mut log = Vec::new();
        let msgs = self.object_messages(addr)?;
        for m in msgs.iter().filter(|m| m.typ == MSG_ATTR_INFO) {
            let mut c = Cur::new(&self.data, m.off);
            let _ver = c.u8()?;
            let flags = c.u8()?;
            if flags & 0x01 != 0 {
                c.skip(2);
            }
            let heap_addr = self.offset_at(&mut c)?;
            let heap = heap::FractalHeap::open(self, heap_addr)?;
            log.push(format!(
                "heap@{heap_addr} id_len={} flags={} width={} start={} max_direct={} cur_rows={} off_bytes={} max_direct_rows={}",
                heap.heap_id_len, heap.flags, heap.table_width, heap.start_block_size,
                heap.max_direct_block_size, heap.cur_rows, heap.offset_bytes, heap.max_direct_rows
            ));
            let areas = heap.object_areas(self)?;
            log.push(format!("direct blocks: {}", areas.len()));
            for (bi, (start, end)) in areas.iter().enumerate() {
                log.push(format!(
                    "-- block {bi}: {start}..{end} ({} bytes)",
                    end - start
                ));
                let mut c = Cur::new(&self.data, *start);
                let mut n = 0;
                while c.p < *end {
                    let p0 = c.p;
                    match self.parse_attribute(&mut c) {
                        Ok(Some((k, _))) => {
                            log.push(format!("   [{n}] @{p0} consumed {} name={k}", c.p - p0));
                            n += 1;
                        }
                        Ok(None) => {
                            log.push(format!("   stop(None) @{p0}: {}", hex(&self.data, p0, 48)));
                            break;
                        }
                        Err(e) => {
                            log.push(format!("   stop({e}) @{p0}: {}", hex(&self.data, p0, 48)));
                            break;
                        }
                    }
                }
            }
        }
        Ok(log)
    }

    fn parse_attribute(&self, c: &mut Cur) -> Result<Option<(String, AttrValue)>> {
        let ver = c.u8()?;
        if ver == 0 || ver > 3 {
            return Ok(None);
        }
        let _flags = c.u8()?;
        let name_size = c.u16()? as usize;
        let dt_size = c.u16()? as usize;
        let ds_size = c.u16()? as usize;
        if ver == 3 {
            c.skip(1); // name character set
        }
        if name_size == 0 || name_size > 4096 || dt_size == 0 || ds_size == 0 {
            return Ok(None);
        }
        // Version 1 pads each section to an 8-byte boundary; later versions do not.
        let pad = |n: usize| if ver == 1 { (8 - (n % 8)) % 8 } else { 0 };

        let name = String::from_utf8_lossy(c.bytes(name_size)?)
            .trim_end_matches('\0')
            .to_string();
        c.skip(pad(name_size));

        let dt = Datatype::parse(&mut Cur::new(&self.data, c.p))?;
        c.skip(dt_size);
        c.skip(pad(dt_size));

        let count = self.dataspace_count(&mut Cur::new(&self.data, c.p))?;
        c.skip(ds_size);
        c.skip(pad(ds_size));

        let nbytes = (count as usize).saturating_mul(dt.size as usize);
        // The data section is never padded, not even in version 1.
        let data = c.bytes(nbytes)?;

        Ok(Some((name, self.decode(&dt, data, count as usize)?)))
    }

    /// Total number of elements described by a dataspace message.
    pub(crate) fn dataspace_count(&self, c: &mut Cur) -> Result<u64> {
        let ver = c.u8()?;
        let rank = c.u8()? as usize;
        let flags = c.u8()?;
        match ver {
            1 => c.skip(5),
            2 => c.skip(1),
            v => return err(format!("unsupported dataspace version {v}")),
        }
        let mut n: u64 = 1;
        for _ in 0..rank {
            n = n.saturating_mul(self.length_at(c)?);
        }
        if flags & 0x01 != 0 {
            for _ in 0..rank {
                let _max = self.length_at(c)?;
            }
        }
        Ok(n)
    }

    /// Dimensions described by a dataspace message.
    pub(crate) fn dataspace_dims(&self, c: &mut Cur) -> Result<Vec<u64>> {
        let ver = c.u8()?;
        let rank = c.u8()? as usize;
        let _flags = c.u8()?;
        match ver {
            1 => c.skip(5),
            2 => c.skip(1),
            v => return err(format!("unsupported dataspace version {v}")),
        }
        let mut dims = Vec::with_capacity(rank);
        for _ in 0..rank {
            dims.push(self.length_at(c)?);
        }
        Ok(dims)
    }

    /// Turn raw element bytes into a typed value.
    pub(crate) fn decode(&self, dt: &Datatype, raw: &[u8], count: usize) -> Result<AttrValue> {
        use dtype::DatatypeClass as C;
        match dt.class {
            C::String => Ok(AttrValue::Text(
                String::from_utf8_lossy(raw)
                    .trim_end_matches('\0')
                    .trim()
                    .to_string(),
            )),
            C::VariableLength if dt.vlen_string => {
                let esz = dt.size as usize;
                let mut parts: Vec<String> = Vec::new();
                for i in 0..count {
                    let base = i * esz;
                    if base + esz > raw.len() {
                        break;
                    }
                    let mut c = Cur::new(raw, base);
                    let _len = c.u32()?;
                    let addr = self.offset_at(&mut c)?;
                    let idx = c.u32()?;
                    if let Ok(bytes) = heap::global_heap_object(self, addr, idx) {
                        parts.push(
                            String::from_utf8_lossy(&bytes)
                                .trim_end_matches('\0')
                                .to_string(),
                        );
                    }
                }
                Ok(AttrValue::Text(parts.join(" ")))
            }
            C::FixedPoint | C::Enumerated => Ok(AttrValue::Ints(read_ints(raw, dt, count))),
            C::Float => Ok(AttrValue::Floats(read_floats(raw, dt, count))),
            _ => Ok(AttrValue::Raw(raw.to_vec())),
        }
    }
}

pub fn read_ints(raw: &[u8], dt: &Datatype, count: usize) -> Vec<i64> {
    let sz = dt.size as usize;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let s = i * sz;
        if s + sz > raw.len() {
            break;
        }
        let mut buf = [0u8; 8];
        if dt.little_endian {
            buf[..sz].copy_from_slice(&raw[s..s + sz]);
        } else {
            for (j, b) in raw[s..s + sz].iter().rev().enumerate() {
                buf[j] = *b;
            }
        }
        let mut v = u64::from_le_bytes(buf);
        if dt.signed && sz < 8 {
            let shift = 64 - sz * 8;
            v = (((v << shift) as i64) >> shift) as u64;
        }
        out.push(v as i64);
    }
    out
}

pub fn read_floats(raw: &[u8], dt: &Datatype, count: usize) -> Vec<f64> {
    let sz = dt.size as usize;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let s = i * sz;
        if s + sz > raw.len() {
            break;
        }
        let mut buf = [0u8; 8];
        if dt.little_endian {
            buf[..sz].copy_from_slice(&raw[s..s + sz]);
        } else {
            for (j, b) in raw[s..s + sz].iter().rev().enumerate() {
                buf[j] = *b;
            }
        }
        out.push(match sz {
            4 => f32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as f64,
            8 => f64::from_le_bytes(buf),
            _ => continue,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Datasets
// ---------------------------------------------------------------------------

pub struct Dataset {
    pub dims: Vec<u64>,
    pub dtype: Datatype,
    pub layout: Layout,
    pub filters: Vec<FilterDef>,
    pub attrs: HashMap<String, AttrValue>,
}

impl Dataset {
    /// Number of elements, saturating rather than wrapping.
    ///
    /// The dimensions are read out of the file, so their product is whatever
    /// the file says. A plain `product()` wraps on overflow, which turns an
    /// absurd shape into a small plausible one - and then into an allocation
    /// and a copy of the wrong length. Saturating keeps an absurd shape absurd,
    /// so the size check in `read_raw` can refuse it.
    pub fn elem_count(&self) -> usize {
        self.dims
            .iter()
            .try_fold(1usize, |acc, &d| acc.checked_mul(usize::try_from(d).ok()?))
            .unwrap_or(usize::MAX)
    }
}

struct ChunkRec {
    addr: u64,
    size: u32,
    offset: Vec<u64>,
}

impl H5File {
    /// Open the dataset whose object header lives at `addr`.
    pub fn dataset(&self, addr: u64) -> Result<Dataset> {
        let msgs = self.object_messages(addr)?;

        let ds_msg = msgs
            .iter()
            .find(|m| m.typ == MSG_DATASPACE)
            .ok_or_else(|| Error("dataset has no dataspace".into()))?;
        let dims = self.dataspace_dims(&mut Cur::new(&self.data, ds_msg.off))?;

        let dt_msg = msgs
            .iter()
            .find(|m| m.typ == MSG_DATATYPE)
            .ok_or_else(|| Error("dataset has no datatype".into()))?;
        let dtype = Datatype::parse(&mut Cur::new(&self.data, dt_msg.off))?;

        let lay_msg = msgs
            .iter()
            .find(|m| m.typ == MSG_LAYOUT)
            .ok_or_else(|| Error("dataset has no layout".into()))?;
        let layout = self.parse_layout(&mut Cur::new(&self.data, lay_msg.off))?;

        let mut filters = Vec::new();
        if let Some(fm) = msgs.iter().find(|m| m.typ == MSG_FILTER) {
            filters = self.parse_filters(&mut Cur::new(&self.data, fm.off))?;
        }

        Ok(Dataset {
            dims,
            dtype,
            layout,
            filters,
            attrs: self.attributes(addr)?,
        })
    }

    fn parse_layout(&self, c: &mut Cur) -> Result<Layout> {
        let ver = c.u8()?;
        if ver != 3 && ver != 4 {
            return err(format!("unsupported data layout version {ver}"));
        }
        let class = c.u8()?;
        match class {
            0 => {
                let size = c.u16()? as usize;
                Ok(Layout::Compact {
                    off: c.p,
                    len: size,
                })
            }
            1 => {
                let addr = self.offset_at(c)?;
                let size = self.length_at(c)?;
                Ok(Layout::Contiguous { addr, size })
            }
            2 => {
                if ver != 3 {
                    return err("only version 3 chunked layout is supported");
                }
                // The stored rank counts one extra dimension for the element size.
                let n = c.u8()? as usize;
                let addr = self.offset_at(c)?;
                let mut dims = Vec::with_capacity(n);
                for _ in 0..n {
                    dims.push(c.u32()?);
                }
                let elem = dims.pop().unwrap_or(1);
                Ok(Layout::Chunked { addr, dims, elem })
            }
            other => err(format!("unknown layout class {other}")),
        }
    }

    fn parse_filters(&self, c: &mut Cur) -> Result<Vec<FilterDef>> {
        let ver = c.u8()?;
        let n = c.u8()? as usize;
        if ver == 1 {
            c.skip(6);
        } else if ver != 2 {
            return err(format!("unsupported filter pipeline version {ver}"));
        }
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let id = c.u16()?;
            let name_len = if ver == 1 || id >= 256 {
                c.u16()? as usize
            } else {
                0
            };
            let flags = c.u16()?;
            let nclient = c.u16()? as usize;
            if name_len > 0 {
                let padded = if ver == 1 {
                    name_len.div_ceil(8) * 8
                } else {
                    name_len
                };
                c.skip(padded);
            }
            c.skip(nclient * 4);
            if ver == 1 && nclient % 2 == 1 {
                c.skip(4); // client data padded to a multiple of 8 bytes
            }
            out.push(FilterDef {
                id,
                optional: flags & 0x01 != 0,
            });
        }
        Ok(out)
    }

    /// Read a dataset's full array as raw little-endian elements, row-major.
    pub fn read_raw(&self, ds: &Dataset) -> Result<Vec<u8>> {
        let elem = ds.dtype.size as usize;
        let total = ds.elem_count();

        /* The allocation below is sized entirely from the file. An absurd shape
        must be refused here rather than handed to the allocator: a failed
        allocation aborts the process, which would take the server down with
        it, and there is no catching that. The largest array these products
        actually hold is a full disc at 3712 squared, about 27 MB. */
        let bytes = total
            .checked_mul(elem)
            .filter(|n| *n <= MAX_DATASET_BYTES)
            .ok_or_else(|| {
                Error(format!(
                    "dataset claims {total} elements of {elem} bytes, beyond the {} MB limit",
                    MAX_DATASET_BYTES / (1024 * 1024)
                ))
            })?;
        let mut out = vec![0u8; bytes];

        match &ds.layout {
            Layout::Compact { off, len } => {
                // Both come from the file and are not otherwise checked. The
                // start is clamped as well as the length: an out-of-range start
                // panics even for an empty range.
                let o = (*off).min(self.data.len());
                let n = (*len).min(out.len()).min(self.data.len() - o);
                out[..n].copy_from_slice(&self.data[o..o + n]);
            }
            Layout::Contiguous { addr, size } => {
                if is_undef(*addr, self.off_size) {
                    return Ok(out); // never written; fill value applies
                }
                let a = (*addr as usize).min(self.data.len());
                // Saturating: an address past the end must give an empty read,
                // not a subtraction that wraps to an enormous length.
                let n = (*size as usize).min(out.len()).min(self.data.len() - a);
                out[..n].copy_from_slice(&self.data[a..a + n]);
            }
            Layout::Chunked { addr, dims, .. } => {
                if is_undef(*addr, self.off_size) {
                    return Ok(out);
                }
                let rank = ds.dims.len();
                if dims.len() != rank {
                    return err("chunk rank does not match dataspace rank");
                }
                // Every index below is relative to the last dimension, so a
                // scalar dataspace would subtract from zero.
                if rank == 0 {
                    return err("chunked layout on a scalar dataspace");
                }
                let mut chunks = Vec::new();
                self.walk_btree1(*addr, rank, &mut chunks)?;

                // Row-major strides over the destination array.
                let mut stride = vec![1usize; rank];
                for d in (0..rank.saturating_sub(1)).rev() {
                    stride[d] = stride[d + 1] * ds.dims[d + 1] as usize;
                }

                let cshape: Vec<usize> = dims.iter().map(|&x| x as usize).collect();
                let inner = cshape[rank - 1];
                let outer: usize = cshape[..rank - 1].iter().product();

                for ch in &chunks {
                    let a = ch.addr as usize;
                    if a + ch.size as usize > self.data.len() {
                        continue;
                    }
                    let raw = self.data[a..a + ch.size as usize].to_vec();
                    let buf = filter::decode(&ds.filters, raw, elem)?;

                    // Walk every row of the chunk and copy the clipped run.
                    for o in 0..outer {
                        let mut rem = o;
                        let mut dst = 0usize;
                        let mut inside = true;
                        for d in (0..rank - 1).rev() {
                            let local = rem % cshape[d];
                            rem /= cshape[d];
                            let g = ch.offset[d] as usize + local;
                            if g >= ds.dims[d] as usize {
                                inside = false;
                                break;
                            }
                            dst += g * stride[d];
                        }
                        if !inside {
                            continue;
                        }
                        let gcol = ch.offset[rank - 1] as usize;
                        let width = ds.dims[rank - 1] as usize;
                        if gcol >= width {
                            continue;
                        }
                        let run = inner.min(width - gcol);
                        let src = o * inner * elem;
                        if src + run * elem > buf.len() {
                            continue;
                        }
                        let d0 = (dst + gcol) * elem;
                        if d0 + run * elem > out.len() {
                            continue;
                        }
                        out[d0..d0 + run * elem].copy_from_slice(&buf[src..src + run * elem]);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Collect every leaf entry of a version 1 chunk B-tree.
    fn walk_btree1(&self, addr: u64, rank: usize, out: &mut Vec<ChunkRec>) -> Result<()> {
        let mut c = Cur::new(&self.data, addr as usize);
        c.tag(b"TREE")?;
        let node_type = c.u8()?;
        if node_type != 1 {
            return err(format!(
                "expected a chunk B-tree, found node type {node_type}"
            ));
        }
        let level = c.u8()?;
        let nused = c.u16()? as usize;
        let _left = self.offset_at(&mut c)?;
        let _right = self.offset_at(&mut c)?;

        for _ in 0..nused {
            let size = c.u32()?;
            let _mask = c.u32()?;
            let mut offset = Vec::with_capacity(rank);
            for _ in 0..=rank {
                offset.push(c.u64()?);
            }
            offset.truncate(rank); // trailing entry is the element offset
            let child = self.offset_at(&mut c)?;
            if level == 0 {
                out.push(ChunkRec {
                    addr: child,
                    size,
                    offset,
                });
            } else {
                self.walk_btree1(child, rank, out)?;
            }
        }
        Ok(())
    }

    /// Read a dataset as unsigned bytes.
    pub fn read_u8(&self, ds: &Dataset) -> Result<Vec<u8>> {
        if ds.dtype.size != 1 {
            return err("dataset is not a single-byte type");
        }
        self.read_raw(ds)
    }

    /// Read a dataset as f64, applying `scale_factor` and `add_offset` when present.
    pub fn read_scaled(&self, ds: &Dataset) -> Result<Vec<f64>> {
        let raw = self.read_raw(ds)?;
        let n = ds.elem_count();
        let mut vals = match ds.dtype.class {
            DatatypeClass::Float => read_floats(&raw, &ds.dtype, n),
            _ => read_ints(&raw, &ds.dtype, n)
                .into_iter()
                .map(|v| v as f64)
                .collect(),
        };
        let scale = ds.attrs.get("scale_factor").and_then(|v| v.as_f64());
        let offset = ds.attrs.get("add_offset").and_then(|v| v.as_f64());
        if scale.is_some() || offset.is_some() {
            let s = scale.unwrap_or(1.0);
            let o = offset.unwrap_or(0.0);
            for v in &mut vals {
                *v = *v * s + o;
            }
        }
        Ok(vals)
    }
}

#[cfg(test)]
mod audit_tests {
    use super::*;

    const SIG: [u8; 8] = [0x89, b'H', b'D', b'F', 0x0d, 0x0a, 0x1a, 0x0a];

    fn dtype(size: u32) -> Datatype {
        Datatype {
            class: DatatypeClass::FixedPoint,
            size,
            signed: false,
            little_endian: true,
            base: None,
            vlen_string: false,
        }
    }

    fn file(fill: u8) -> H5File {
        let mut data = vec![fill; 64];
        data[0..8].copy_from_slice(&SIG);
        H5File {
            data,
            off_size: 8,
            len_size: 8,
            root_addr: 0,
        }
    }

    fn dataset(dims: Vec<u64>, size: u32, layout: Layout) -> Dataset {
        Dataset {
            dims,
            dtype: dtype(size),
            layout,
            filters: Vec::new(),
            attrs: HashMap::new(),
        }
    }

    /// Dimensions come out of the file, so their product is whatever the file
    /// says. Wrapping would turn an absurd shape into a plausible one and then
    /// into an allocation and a copy of the wrong length.
    #[test]
    fn element_count_saturates_rather_than_wrapping() {
        let c =
            |dims: Vec<u64>| dataset(dims, 8, Layout::Contiguous { addr: 0, size: 0 }).elem_count();
        assert_eq!(c(vec![1 << 40, 1 << 40]), usize::MAX, "2^80 wrapped");
        assert_eq!(
            c(vec![3, 1 << 63]),
            usize::MAX,
            "wrapped to a plausible size"
        );
        assert_eq!(
            c(vec![3712, 3712]),
            3712 * 3712,
            "a real shape must survive"
        );
    }

    /// An absurd shape must be refused, not handed to the allocator: a failed
    /// allocation aborts the process rather than returning an error, and there
    /// is no catching that.
    #[test]
    fn an_absurd_shape_is_refused_before_allocating() {
        let f = file(0);
        let ds = dataset(
            vec![1 << 40, 1 << 40],
            8,
            Layout::Contiguous { addr: 0, size: 16 },
        );
        let e = f.read_raw(&ds).unwrap_err();
        assert!(e.0.contains("limit"), "unexpected error: {}", e.0);
    }

    /// Offsets and addresses are read from the file and may point anywhere.
    /// Past the end they must read nothing, not subtract below zero.
    #[test]
    fn layout_offsets_past_the_end_read_nothing() {
        let f = file(7);
        for layout in [
            Layout::Contiguous {
                addr: 1_000_000,
                size: 32,
            },
            Layout::Compact {
                off: 1_000_000,
                len: 32,
            },
        ] {
            let out = f
                .read_raw(&dataset(vec![32], 1, layout))
                .expect("must not panic");
            assert_eq!(out.len(), 32);
            assert!(out.iter().all(|b| *b == 0), "read from out of bounds");
        }
    }

    /// The chunk walk indexes relative to the last dimension, so a scalar
    /// dataspace would subtract from zero.
    #[test]
    fn a_scalar_chunked_dataspace_is_refused() {
        let f = file(0);
        let ds = dataset(
            Vec::new(),
            1,
            Layout::Chunked {
                addr: 16,
                dims: Vec::new(),
                elem: 1,
            },
        );
        assert!(f.read_raw(&ds).is_err());
    }
}
