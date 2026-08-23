//! Hand-rolled thrift compact-protocol PAGE HEADERS plus a parallel page
//! decoder for 8-byte primitive columns. The parquet crate keeps its header
//! parser private and its page reader decompresses serially; walking the
//! ~30-byte headers ourselves turns a column chunk into independent
//! (header, bytes) pairs that decompress and decode on rayon. Any surprise —
//! an unknown codec, encoding, or header shape — answers `None` and the
//! caller falls back to the crate's readers, so this path can only ever be
//! faster, never wronger.

use parquet::basic::Compression;
use parquet::file::metadata::ParquetMetaData;
use rayon::prelude::*;

use super::rle;

/// A decoded primitive column: values plus validity.
type PrimCol<T> = (Vec<T>, Vec<bool>);

// ---- thrift compact protocol: just enough to read a PageHeader ----

struct Cur<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Cur<'_> {
    fn u8(&mut self) -> Result<u8, String> {
        let v = *self.b.get(self.pos).ok_or("page header ends early")?;
        self.pos += 1;
        Ok(v)
    }

    fn varint(&mut self) -> Result<u64, String> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let b = self.u8()?;
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
            if shift > 63 {
                return Err("varint overflow".to_string());
            }
        }
    }

    fn zigzag(&mut self) -> Result<i64, String> {
        let v = self.varint()?;
        Ok((v >> 1) as i64 ^ -((v & 1) as i64))
    }

    fn skip_bytes(&mut self, n: usize) -> Result<(), String> {
        if self.pos + n > self.b.len() {
            return Err("page header ends early".to_string());
        }
        self.pos += n;
        Ok(())
    }

    /// Skip one field payload of the given compact-protocol type.
    fn skip(&mut self, ty: u8) -> Result<(), String> {
        match ty {
            1 | 2 => Ok(()), // bool: the value lives in the type nibble
            3 => self.skip_bytes(1),
            4..=6 => self.zigzag().map(|_| ()),
            7 => self.skip_bytes(8),
            8 => {
                let n = self.varint()? as usize;
                self.skip_bytes(n)
            }
            9 | 10 => {
                let h = self.u8()?;
                let mut n = (h >> 4) as usize;
                let et = h & 0x0f;
                if n == 15 {
                    n = self.varint()? as usize;
                }
                for _ in 0..n {
                    self.skip_elem(et)?;
                }
                Ok(())
            }
            11 => {
                let n = self.varint()? as usize;
                if n == 0 {
                    return Ok(());
                }
                let kv = self.u8()?;
                for _ in 0..n {
                    self.skip_elem(kv >> 4)?;
                    self.skip_elem(kv & 0x0f)?;
                }
                Ok(())
            }
            12 => self.skip_struct(),
            _ => Err(format!("unknown thrift type {ty}")),
        }
    }

    /// List/map elements: bools take a byte there, everything else as `skip`.
    fn skip_elem(&mut self, ty: u8) -> Result<(), String> {
        match ty {
            1 | 2 => self.skip_bytes(1),
            _ => self.skip(ty),
        }
    }

    fn skip_struct(&mut self) -> Result<(), String> {
        loop {
            let h = self.u8()?;
            if h == 0 {
                return Ok(());
            }
            if h >> 4 == 0 {
                self.zigzag()?; // long-form field id
            }
            self.skip(h & 0x0f)?;
        }
    }
}

/// The fields of a PageHeader this decoder acts on. Encodings are the
/// parquet.thrift numbers: PLAIN=0, PLAIN_DICTIONARY=2, RLE=3,
/// RLE_DICTIONARY=8; page types DATA=0, DICTIONARY=2, DATA_V2=3.
struct PageHead {
    page_type: i32,
    uncompressed: usize,
    compressed: usize,
    num_values: usize,
    encoding: i32,
    def_encoding: i32,
    header_len: usize,
}

fn parse_page_head(b: &[u8]) -> Result<PageHead, String> {
    let mut c = Cur { b, pos: 0 };
    let mut h = PageHead {
        page_type: -1,
        uncompressed: 0,
        compressed: 0,
        num_values: 0,
        encoding: -1,
        def_encoding: -1,
        header_len: 0,
    };
    let mut fid: i64 = 0;
    loop {
        let fb = c.u8()?;
        if fb == 0 {
            break;
        }
        let ty = fb & 0x0f;
        let delta = (fb >> 4) as i64;
        fid = if delta == 0 { c.zigzag()? } else { fid + delta };
        match (fid, ty) {
            (1, 5) => h.page_type = c.zigzag()? as i32,
            (2, 5) => h.uncompressed = c.zigzag()? as usize,
            (3, 5) => h.compressed = c.zigzag()? as usize,
            // DataPageHeader / DictionaryPageHeader: both put num_values at 1
            // and encoding at 2; the def-level encoding is data-page field 3.
            (5, 12) | (7, 12) => {
                let mut sid: i64 = 0;
                loop {
                    let sb = c.u8()?;
                    if sb == 0 {
                        break;
                    }
                    let sty = sb & 0x0f;
                    let sd = (sb >> 4) as i64;
                    sid = if sd == 0 { c.zigzag()? } else { sid + sd };
                    match (sid, sty) {
                        (1, 5) => h.num_values = c.zigzag()? as usize,
                        (2, 5) => h.encoding = c.zigzag()? as i32,
                        (3, 5) if fid == 5 => h.def_encoding = c.zigzag()? as i32,
                        _ => c.skip(sty)?,
                    }
                }
            }
            _ => c.skip(ty)?,
        }
    }
    h.header_len = c.pos;
    Ok(h)
}

// ---- the parallel primitive decoder ----

/// One data page, sliced out of its chunk and ready to decode independently.
struct PagePlan<'a> {
    rg: usize,
    n: usize,
    encoding: i32,
    uncompressed: usize,
    zstd: bool,
    data: &'a [u8],
}

fn arr8(b: &[u8], at: usize) -> Result<[u8; 8], String> {
    b.get(at..at + 8)
        .map(|s| [s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]])
        .ok_or_else(|| "page ends inside a value".to_string())
}

/// Walk every row group's column chunk into per-row-group dictionaries plus
/// independent data-page plans. `None` = a shape the page path doesn't cover.
#[allow(clippy::type_complexity)]
fn plan_pages<'a, T: Copy>(
    bytes: &'a bytes::Bytes,
    meta: &ParquetMetaData,
    ci: usize,
    optional: bool,
    rows: usize,
    from_le: &impl Fn([u8; 8]) -> T,
) -> Result<Option<(Vec<Vec<T>>, Vec<PagePlan<'a>>)>, String> {
    let mut dicts: Vec<Vec<T>> = Vec::new();
    let mut plans: Vec<PagePlan> = Vec::new();
    for rg_idx in 0..meta.num_row_groups() {
        let cc = meta.row_group(rg_idx).column(ci);
        let zstd = match cc.compression() {
            Compression::ZSTD(_) => true,
            Compression::UNCOMPRESSED => false,
            _ => return Ok(None),
        };
        let start = cc.dictionary_page_offset().unwrap_or_else(|| cc.data_page_offset());
        let (start, len) = (start as usize, cc.compressed_size() as usize);
        let Some(chunk) = bytes.get(start..start + len) else {
            return Ok(None);
        };
        let mut dict: Vec<T> = Vec::new();
        let mut pos = 0usize;
        while pos < chunk.len() {
            let Ok(h) = parse_page_head(&chunk[pos..]) else {
                return Ok(None);
            };
            let Some(data) = chunk.get(pos + h.header_len..pos + h.header_len + h.compressed)
            else {
                return Ok(None);
            };
            pos += h.header_len + h.compressed;
            match h.page_type {
                2 => {
                    // Dictionary page: PLAIN values, 8 bytes each.
                    if h.encoding != 0 && h.encoding != 2 {
                        return Ok(None);
                    }
                    let raw: Vec<u8>;
                    let buf: &[u8] = if zstd {
                        raw = zstd::bulk::decompress(data, h.uncompressed)
                            .map_err(|e| format!("page decompress: {e}"))?;
                        &raw
                    } else {
                        data
                    };
                    dict.clear();
                    dict.reserve(h.num_values);
                    for i in 0..h.num_values {
                        dict.push(from_le(arr8(buf, i * 8)?));
                    }
                }
                0 => {
                    if optional && h.def_encoding != 3 {
                        return Ok(None); // def levels must be RLE
                    }
                    plans.push(PagePlan {
                        rg: rg_idx,
                        n: h.num_values,
                        encoding: h.encoding,
                        uncompressed: h.uncompressed,
                        zstd,
                        data,
                    });
                }
                _ => return Ok(None),
            }
        }
        dicts.push(dict);
    }
    for p in &plans {
        match p.encoding {
            0 => {}
            2 | 8 => {
                if dicts[p.rg].is_empty() {
                    return Ok(None);
                }
            }
            _ => return Ok(None),
        }
    }
    if plans.iter().map(|p| p.n).sum::<usize>() != rows {
        return Ok(None);
    }
    Ok(Some((dicts, plans)))
}

/// Split `mask` into per-plan windows (each page fills its own rows).
fn mask_windows<'m>(mask: &'m mut [bool], plans: &[PagePlan]) -> Vec<&'m mut [bool]> {
    let mut rest = mask;
    let mut out = Vec::with_capacity(plans.len());
    for p in plans {
        let (w, r) = rest.split_at_mut(p.n);
        rest = r;
        out.push(w);
    }
    out
}

/// Decode an INT64/DOUBLE column by walking page headers directly and
/// decompressing + decoding every data page on rayon. `None` = a shape this
/// path doesn't cover (codec, encoding, V2 pages) — use the crate readers.
pub(crate) fn read_prim<T: Copy + Send + Sync>(
    bytes: &bytes::Bytes,
    meta: &ParquetMetaData,
    ci: usize,
    optional: bool,
    rows: usize,
    zero: T,
    from_le: impl Fn([u8; 8]) -> T + Sync,
) -> Result<Option<PrimCol<T>>, String> {
    let Some((dicts, plans)) = plan_pages(bytes, meta, ci, optional, rows, &from_le)? else {
        return Ok(None);
    };

    // Preallocate the column and hand each page its disjoint window.
    let mut vals: Vec<T> = vec![zero; rows];
    let mut valid: Vec<bool> = vec![true; rows];
    let mut windows: Vec<(&mut [T], &mut [bool])> = Vec::with_capacity(plans.len());
    {
        let mut rest_v: &mut [T] = &mut vals;
        let mut rest_k: &mut [bool] = &mut valid;
        for p in &plans {
            let (v, rv) = rest_v.split_at_mut(p.n);
            let (k, rk) = rest_k.split_at_mut(p.n);
            rest_v = rv;
            rest_k = rk;
            windows.push((v, k));
        }
    }
    let dicts = &dicts;
    let from_le = &from_le;
    let first_err: Option<String> = plans
        .par_iter()
        .zip(windows.into_par_iter())
        .filter_map(|(p, (vw, kw))| {
            decode_page(p, &dicts[p.rg], optional, from_le, vw, kw).err()
        })
        .reduce_with(|a, _| a);
    match first_err {
        Some(e) => Err(e),
        None => Ok(Some((vals, valid))),
    }
}

/// Fill one page's window: decompress, read def levels, then either map
/// dictionary codes or copy PLAIN values. Missing rows keep the prefilled
/// zero sentinel; `kw` starts all-true and only an optional column rewrites.
fn decode_page<T: Copy>(
    p: &PagePlan,
    dict: &[T],
    optional: bool,
    from_le: &(impl Fn([u8; 8]) -> T + Sync),
    vw: &mut [T],
    kw: &mut [bool],
) -> Result<(), String> {
    let raw: Vec<u8>;
    let buf: &[u8] = if p.zstd {
        raw = zstd::bulk::decompress(p.data, p.uncompressed)
            .map_err(|e| format!("page decompress: {e}"))?;
        &raw
    } else {
        p.data
    };
    let n = p.n;
    let mut pos = 0usize;
    let mut present = n;
    if optional {
        let lenb = buf.get(0..4).ok_or("data page ends inside the level length")?;
        let dlen = u32::from_le_bytes([lenb[0], lenb[1], lenb[2], lenb[3]]) as usize;
        let mut levels: Vec<u32> = Vec::with_capacity(n);
        rle::decode(buf.get(4..4 + dlen).ok_or("data page ends inside levels")?, 1, n, &mut levels)?;
        pos = 4 + dlen;
        present = 0;
        for (j, l) in levels.iter().enumerate() {
            kw[j] = *l == 1;
            if *l == 1 {
                present += 1;
            }
        }
    }
    if p.encoding == 0 {
        // PLAIN: values back to back, present rows only.
        let mut at = pos;
        for j in 0..n {
            if kw[j] {
                vw[j] = from_le(arr8(buf, at)?);
                at += 8;
            }
        }
        return Ok(());
    }
    // Dictionary codes: [u8 bit width][RLE/bit-packed codes].
    let width = *buf.get(pos).ok_or("data page ends before the bit width")?;
    if width > 32 {
        return Err("dictionary index width exceeds 32 bits".to_string());
    }
    let mut codes: Vec<u32> = Vec::with_capacity(present);
    rle::decode(buf.get(pos + 1..).ok_or("data page ends inside the codes")?, width, present, &mut codes)?;
    if present == n {
        for (j, &code) in codes.iter().enumerate() {
            vw[j] = *dict.get(code as usize).ok_or("dictionary code out of range")?;
        }
    } else {
        let mut c = codes.iter();
        for j in 0..n {
            if kw[j] {
                let &code = c.next().ok_or("fewer codes than present values")?;
                vw[j] = *dict.get(code as usize).ok_or("dictionary code out of range")?;
            }
        }
    }
    Ok(())
}

/// Evaluate `col OP literal` straight off the pages: the predicate runs once
/// per DISTINCT dictionary value, then each page's mask window is a table
/// lookup per code — the column itself is never materialized. Missing rows
/// stay false, matching the filter's missing-keeps-the-row-out rule. `None` =
/// fall back to decoding the column (uncovered shape, or a NaN anywhere the
/// serial walk's error semantics would need to see).
#[allow(clippy::too_many_arguments)] // the two closures are the API; a config struct would only rename them
pub(crate) fn filter_prim<T: Copy + Send + Sync>(
    bytes: &bytes::Bytes,
    meta: &ParquetMetaData,
    ci: usize,
    optional: bool,
    rows: usize,
    from_le: impl Fn([u8; 8]) -> T + Sync,
    pred: impl Fn(T) -> bool + Sync,
    is_nan: impl Fn(T) -> bool + Sync,
) -> Result<Option<(Vec<bool>, usize)>, String> {
    let Some((dicts, plans)) = plan_pages(bytes, meta, ci, optional, rows, &from_le)? else {
        return Ok(None);
    };
    // Predicate per distinct value. A NaN in a dictionary defers to the flat
    // path, whose row-by-row walk owns the NaN error.
    let mut tables: Vec<Vec<bool>> = Vec::with_capacity(dicts.len());
    for dict in &dicts {
        if dict.iter().any(|v| is_nan(*v)) {
            return Ok(None);
        }
        tables.push(dict.iter().map(|v| pred(*v)).collect());
    }
    let mut mask = vec![false; rows];
    let windows = mask_windows(&mut mask, &plans);
    let tables = &tables;
    let from_le = &from_le;
    let pred = &pred;
    let is_nan = &is_nan;
    let per_page: Vec<Result<Option<usize>, String>> = plans
        .par_iter()
        .zip(windows.into_par_iter())
        .map(|(p, mw)| filter_page(p, &tables[p.rg], optional, from_le, pred, is_nan, mw))
        .collect();
    let mut n = 0usize;
    for r in per_page {
        match r? {
            Some(cnt) => n += cnt,
            None => return Ok(None), // a PLAIN page met a NaN: flat path
        }
    }
    Ok(Some((mask, n)))
}

/// One page's mask window: dictionary pages look each code up in the
/// predicate table; PLAIN pages evaluate values directly (bailing to the
/// flat path on any NaN). Returns the window's match count.
fn filter_page<T: Copy>(
    p: &PagePlan,
    table: &[bool],
    optional: bool,
    from_le: &(impl Fn([u8; 8]) -> T + Sync),
    pred: &(impl Fn(T) -> bool + Sync),
    is_nan: &(impl Fn(T) -> bool + Sync),
    mw: &mut [bool],
) -> Result<Option<usize>, String> {
    let raw: Vec<u8>;
    let buf: &[u8] = if p.zstd {
        raw = zstd::bulk::decompress(p.data, p.uncompressed)
            .map_err(|e| format!("page decompress: {e}"))?;
        &raw
    } else {
        p.data
    };
    let n = p.n;
    let mut pos = 0usize;
    let mut present = n;
    let mut levels: Vec<u32> = Vec::new();
    if optional {
        let lenb = buf.get(0..4).ok_or("data page ends inside the level length")?;
        let dlen = u32::from_le_bytes([lenb[0], lenb[1], lenb[2], lenb[3]]) as usize;
        levels.reserve(n);
        rle::decode(buf.get(4..4 + dlen).ok_or("data page ends inside levels")?, 1, n, &mut levels)?;
        pos = 4 + dlen;
        present = levels.iter().filter(|l| **l == 1).count();
    }
    let all_valid = present == n;
    let mut cnt = 0usize;
    if p.encoding == 0 {
        // PLAIN values, present rows only.
        let mut at = pos;
        for j in 0..n {
            if all_valid || levels[j] == 1 {
                let v = from_le(arr8(buf, at)?);
                at += 8;
                if is_nan(v) {
                    return Ok(None);
                }
                if pred(v) {
                    mw[j] = true;
                    cnt += 1;
                }
            }
        }
        return Ok(Some(cnt));
    }
    // Dictionary codes: [u8 bit width][RLE/bit-packed codes].
    let width = *buf.get(pos).ok_or("data page ends before the bit width")?;
    if width > 32 {
        return Err("dictionary index width exceeds 32 bits".to_string());
    }
    let mut codes: Vec<u32> = Vec::with_capacity(present);
    rle::decode(buf.get(pos + 1..).ok_or("data page ends inside the codes")?, width, present, &mut codes)?;
    if all_valid {
        for (j, &code) in codes.iter().enumerate() {
            if *table.get(code as usize).ok_or("dictionary code out of range")? {
                mw[j] = true;
                cnt += 1;
            }
        }
    } else {
        let mut c = codes.iter();
        for j in 0..n {
            if levels[j] == 1 {
                let &code = c.next().ok_or("fewer codes than present values")?;
                if *table.get(code as usize).ok_or("dictionary code out of range")? {
                    mw[j] = true;
                    cnt += 1;
                }
            }
        }
    }
    Ok(Some(cnt))
}
