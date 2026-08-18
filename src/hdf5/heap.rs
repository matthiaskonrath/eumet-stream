//! HDF5 fractal heap (dense link and attribute storage) and global heap
//! (variable-length data).
//!
//! Rather than resolving individual heap IDs through the version 2 B-tree name
//! index, the whole heap is enumerated: every direct block is located through
//! the doubling table and the messages packed inside are parsed in order. These
//! products are written once and never edited, so objects sit contiguously from
//! the start of each block with free space only at the tail.

use super::{err, is_undef, Cur, H5File, Result};

pub struct FractalHeap {
    pub heap_id_len: u16,
    pub filter_enc_len: u16,
    pub flags: u8,
    pub table_width: u16,
    pub start_block_size: u64,
    pub max_direct_block_size: u64,
    pub root_addr: u64,
    pub cur_rows: u16,
    /// Width in bytes of a heap offset, derived from the maximum heap size.
    pub offset_bytes: usize,
    /// Number of doubling-table rows that hold direct blocks.
    pub max_direct_rows: u32,
}

fn log2_floor(v: u64) -> u32 {
    debug_assert!(v > 0);
    63 - v.leading_zeros()
}

impl FractalHeap {
    pub fn open(f: &H5File, addr: u64) -> Result<Self> {
        let mut c = Cur::new(&f.data, addr as usize);
        c.tag(b"FRHP")?;
        let ver = c.u8()?;
        if ver != 0 {
            return err(format!("unsupported fractal heap version {ver}"));
        }
        let heap_id_len = c.u16()?;
        let filter_enc_len = c.u16()?;
        let flags = c.u8()?;
        let _max_man_size = c.u32()?;

        let _next_huge = f.length_at(&mut c)?;
        let _huge_btree = f.offset_at(&mut c)?;
        let _free_space = f.length_at(&mut c)?;
        let _free_mgr = f.offset_at(&mut c)?;
        let _man_space = f.length_at(&mut c)?;
        let _man_alloc = f.length_at(&mut c)?;
        let _next_dir = f.length_at(&mut c)?;
        let _n_managed = f.length_at(&mut c)?;
        let _huge_size = f.length_at(&mut c)?;
        let _n_huge = f.length_at(&mut c)?;
        let _tiny_size = f.length_at(&mut c)?;
        let _n_tiny = f.length_at(&mut c)?;

        let table_width = c.u16()?;
        let start_block_size = f.length_at(&mut c)?;
        let max_direct_block_size = f.length_at(&mut c)?;
        let max_heap_size_bits = c.u16()?;
        let _start_rows = c.u16()?;
        let root_addr = f.offset_at(&mut c)?;
        let cur_rows = c.u16()?;

        if start_block_size == 0 || max_direct_block_size == 0 || table_width == 0 {
            return err("malformed fractal heap doubling table");
        }

        let offset_bytes = (max_heap_size_bits as usize).div_ceil(8);
        let max_direct_rows = log2_floor(max_direct_block_size) - log2_floor(start_block_size) + 2;

        Ok(FractalHeap {
            heap_id_len,
            filter_enc_len,
            flags,
            table_width,
            start_block_size,
            max_direct_block_size,
            root_addr,
            cur_rows,
            offset_bytes,
            max_direct_rows,
        })
    }

    /// Block size for doubling-table row `r`. Rows 0 and 1 share a size; each
    /// subsequent row doubles.
    fn row_block_size(&self, r: u32) -> u64 {
        if r < 2 {
            self.start_block_size
        } else {
            self.start_block_size << (r - 1)
        }
    }

    /// File spans `(start, end)` of the object area of every direct block.
    pub fn object_areas(&self, f: &H5File) -> Result<Vec<(usize, usize)>> {
        let mut out = Vec::new();
        if is_undef(self.root_addr, f.off_size) {
            return Ok(out);
        }
        if self.filter_enc_len > 0 {
            return err("filtered fractal heap direct blocks are not supported");
        }
        if self.cur_rows == 0 {
            self.direct_area(f, self.root_addr, self.start_block_size, &mut out)?;
        } else {
            self.walk_indirect(f, self.root_addr, self.cur_rows as u32, &mut out)?;
        }
        Ok(out)
    }

    fn direct_area(
        &self,
        f: &H5File,
        addr: u64,
        block_size: u64,
        out: &mut Vec<(usize, usize)>,
    ) -> Result<()> {
        let mut c = Cur::new(&f.data, addr as usize);
        c.tag(b"FHDB")?;
        let _ver = c.u8()?;
        let _hdr = f.offset_at(&mut c)?;
        let _block_off = c.var(self.offset_bytes)?;
        if self.flags & 0x02 != 0 {
            c.skip(4); // direct blocks are checksummed
        }
        let start = c.p;
        let end = (addr as usize + block_size as usize).min(f.data.len());
        if start < end {
            out.push((start, end));
        }
        Ok(())
    }

    fn walk_indirect(
        &self,
        f: &H5File,
        addr: u64,
        nrows: u32,
        out: &mut Vec<(usize, usize)>,
    ) -> Result<()> {
        let mut c = Cur::new(&f.data, addr as usize);
        c.tag(b"FHIB")?;
        let _ver = c.u8()?;
        let _hdr = f.offset_at(&mut c)?;
        let _block_off = c.var(self.offset_bytes)?;

        let first_row_bits =
            log2_floor(self.start_block_size) + log2_floor(self.table_width as u64);

        for r in 0..nrows {
            let size = self.row_block_size(r);
            for _ in 0..self.table_width {
                let child = f.offset_at(&mut c)?;
                if r < self.max_direct_rows {
                    if self.filter_enc_len > 0 {
                        let _fsize = f.length_at(&mut c)?;
                        let _mask = c.u32()?;
                    }
                    if !is_undef(child, f.off_size) && child != 0 {
                        self.direct_area(f, child, size, out)?;
                    }
                } else if !is_undef(child, f.off_size) && child != 0 {
                    let child_rows = log2_floor(size) - first_row_bits + 1;
                    self.walk_indirect(f, child, child_rows, out)?;
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Global heap (variable-length data)
// ---------------------------------------------------------------------------

/// Fetch object `index` from the global heap collection at `addr`.
pub fn global_heap_object(f: &H5File, addr: u64, index: u32) -> Result<Vec<u8>> {
    let mut c = Cur::new(&f.data, addr as usize);
    c.tag(b"GCOL")?;
    let ver = c.u8()?;
    if ver != 1 {
        return err(format!("unsupported global heap version {ver}"));
    }
    c.skip(3);
    let collection_size = f.length_at(&mut c)?;
    let end = (addr as usize + collection_size as usize).min(f.data.len());

    while c.p + 16 <= end {
        let idx = c.u16()? as u32;
        let _refcount = c.u16()?;
        c.skip(4);
        let size = f.length_at(&mut c)?;
        if idx == 0 {
            break; // free space terminator
        }
        let data = c.bytes(size as usize)?.to_vec();
        if idx == index {
            return Ok(data);
        }
        // Objects are padded to an 8-byte boundary.
        let pad = (8 - (size % 8)) % 8;
        c.skip(pad as usize);
    }
    err(format!("global heap object {index} not found"))
}
