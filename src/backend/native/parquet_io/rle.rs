//! The parquet RLE / bit-packed hybrid — hand-rolled against the format spec
//! (it is small and frozen), because the crate's own codec sits behind an
//! `experimental` flag that drags the arrow stack in. Decoding is validated
//! differentially: the round-trip tests read chunks the crate's writer encoded,
//! and the crate's reader decodes chunks this encoder produced.
//!
//! Stream = runs. Header = ULEB128 varint H:
//!   H & 1 == 0 → RLE run: (H >> 1) copies of ONE value stored LSB-first in
//!                ceil(bit_width / 8) bytes;
//!   H & 1 == 1 → bit-packed run: (H >> 1) GROUPS of 8 values, `bit_width`
//!                bits each, LSB-first within each byte.

/// Decode `count` values of `bit_width` bits from `data`, appending to `out`.
/// Returns the bytes consumed.
pub fn decode(
    data: &[u8],
    bit_width: u8,
    count: usize,
    out: &mut Vec<u32>,
) -> Result<usize, String> {
    let start = out.len();
    let byte_w = bit_width.div_ceil(8) as usize;
    let mut pos = 0usize;
    while out.len() - start < count {
        // ULEB128 header.
        let mut header: u64 = 0;
        let mut shift = 0;
        loop {
            let b = *data.get(pos).ok_or("RLE stream ends inside a run header")?;
            pos += 1;
            header |= ((b & 0x7F) as u64) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift > 63 {
                return Err("RLE header overflows".to_string());
            }
        }
        if header & 1 == 0 {
            // RLE run.
            let run = (header >> 1) as usize;
            let mut v: u32 = 0;
            for k in 0..byte_w {
                let b = *data.get(pos).ok_or("RLE stream ends inside a run value")?;
                pos += 1;
                v |= (b as u32) << (8 * k);
            }
            let take = run.min(count - (out.len() - start));
            out.extend(std::iter::repeat_n(v, take));
        } else {
            // Bit-packed groups of 8.
            let groups = (header >> 1) as usize;
            let total_bits = groups * 8 * bit_width as usize;
            let bytes = total_bits / 8;
            let chunk = data.get(pos..pos + bytes).ok_or("RLE stream ends inside a group")?;
            pos += bytes;
            let mut bit = 0usize;
            for _ in 0..groups * 8 {
                if out.len() - start >= count {
                    break;
                }
                let mut v: u32 = 0;
                for k in 0..bit_width as usize {
                    let idx = bit + k;
                    if chunk[idx / 8] & (1 << (idx % 8)) != 0 {
                        v |= 1 << k;
                    }
                }
                bit += bit_width as usize;
                out.push(v);
            }
        }
    }
    Ok(pos)
}

/// Encode `values` at `bit_width` bits: runs of eight-or-more identical values
/// become RLE runs; everything else bit-packs in groups of eight. Appends to
/// `out`.
pub fn encode(values: &[u32], bit_width: u8, out: &mut Vec<u8>) {
    let byte_w = bit_width.div_ceil(8) as usize;
    fn push_header(h: u64, out: &mut Vec<u8>) {
        let mut h = h;
        loop {
            let b = (h & 0x7F) as u8;
            h >>= 7;
            if h == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
    }
    // Emit `pending` as bit-packed groups. Mid-stream the length is always a
    // multiple of 8 (see below); only the FINAL flush may pad its tail group —
    // the decoder stops at `count`, and the pad bytes are inside the group it
    // consumes, so the byte accounting stays exact.
    fn flush(pending: &mut Vec<u32>, bit_width: u8, out: &mut Vec<u8>) {
        if pending.is_empty() {
            return;
        }
        while !pending.len().is_multiple_of(8) {
            pending.push(0);
        }
        let groups = pending.len() / 8;
        push_header(((groups as u64) << 1) | 1, out);
        let mut acc: u64 = 0;
        let mut nbits = 0usize;
        for &v in pending.iter() {
            acc |= (v as u64) << nbits;
            nbits += bit_width as usize;
            while nbits >= 8 {
                out.push((acc & 0xFF) as u8);
                acc >>= 8;
                nbits -= 8;
            }
        }
        if nbits > 0 {
            out.push((acc & 0xFF) as u8);
        }
        pending.clear();
    }

    let mut i = 0usize;
    let mut pending: Vec<u32> = Vec::with_capacity(512);
    while i < values.len() {
        let v = values[i];
        let mut j = i + 1;
        while j < values.len() && values[j] == v {
            j += 1;
        }
        let run = j - i;
        if run >= 8 && pending.len().is_multiple_of(8) {
            flush(&mut pending, bit_width, out); // exact multiple — no padding
            push_header((run as u64) << 1, out);
            for k in 0..byte_w {
                out.push(((v >> (8 * k)) & 0xFF) as u8);
            }
            i = j;
        } else if run >= 8 {
            // Borrow from the run to complete the open group, then loop — the
            // remainder re-evaluates (still >= 8 → RLE at a clean boundary).
            let need = 8 - pending.len() % 8;
            pending.extend(std::iter::repeat_n(v, need));
            i += need;
        } else {
            pending.extend_from_slice(&values[i..j]);
            i = j;
            if pending.len() >= 512 {
                // Keep the tail remainder pending so the flush stays exact.
                let keep = pending.len() % 8;
                let mut tail = pending.split_off(pending.len() - keep);
                flush(&mut pending, bit_width, out);
                pending.append(&mut tail);
            }
        }
    }
    flush(&mut pending, bit_width, out); // final — padding allowed
}

/// The narrowest width that can carry `max`.
pub fn width_for(max: u32) -> u8 {
    (32 - max.leading_zeros()).max(1) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(vals: &[u32], w: u8) {
        let mut enc = Vec::new();
        encode(vals, w, &mut enc);
        let mut dec = Vec::new();
        let used = decode(&enc, w, vals.len(), &mut dec).unwrap();
        assert_eq!(used, enc.len());
        assert_eq!(dec, vals);
    }

    #[test]
    fn round_trips_cover_both_run_kinds() {
        round_trip(&[1, 1, 1, 1, 1, 1, 1, 1, 1, 1], 1);
        round_trip(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9], 4);
        round_trip(&[7; 100], 3);
        let mixed: Vec<u32> = (0..1000).map(|i| if i % 37 < 30 { 5 } else { i % 8 }).collect();
        round_trip(&mixed, 6);
        // A long run arriving while a group is open — the borrow path.
        let mut awkward = vec![1u32, 2, 3];
        awkward.extend(std::iter::repeat_n(9, 100));
        awkward.extend([4, 5]);
        awkward.extend(std::iter::repeat_n(2, 20));
        round_trip(&awkward, 4);
        round_trip(&[0], 1);
        round_trip(&[1023, 0, 1023, 5], 10);
    }

    #[test]
    fn width_for_is_exact() {
        assert_eq!(width_for(0), 1);
        assert_eq!(width_for(1), 1);
        assert_eq!(width_for(2), 2);
        assert_eq!(width_for(49), 6);
        assert_eq!(width_for(255), 8);
        assert_eq!(width_for(256), 9);
    }
}
