//! Delta-of-delta, shared by every codec for timestamps and by the decimal codec for values.
//!
//! The widest bucket is 64 bits so an arbitrary gap never needs a special case.

use crate::bits::{fits_in, sign_extend, BitReader, BitWriter};
use crate::error::Result;

/// The original paper stops at 12 bits and then jumps straight to a full 64-bit escape, which
/// is ruinous for series whose second derivative routinely needs 13 to 32 bits — river
/// discharge in whole cubic feet per second, for instance. The extra buckets cost one bit of
/// prefix each to the rare wide values and save up to 48 bits every time one lands.
const BUCKETS: [u32; 7] = [7, 9, 12, 16, 20, 24, 32];

const ESCAPE: u32 = BUCKETS.len() as u32 + 1;

pub fn write(w: &mut BitWriter, dod: i64) {
    if dod == 0 {
        w.write_bit(false);
        return;
    }
    for (index, &width) in BUCKETS.iter().enumerate() {
        if fits_in(dod, width) {
            w.write_bits(u64::MAX, index as u32 + 1);
            w.write_bit(false);
            w.write_bits(dod as u64, width);
            return;
        }
    }
    w.write_bits(u64::MAX, ESCAPE);
    w.write_bits(dod as u64, 64);
}

pub fn read(r: &mut BitReader) -> Result<i64> {
    let mut leading_ones = 0u32;
    while leading_ones < ESCAPE && r.read_bit()? {
        leading_ones += 1;
    }
    if leading_ones == 0 {
        return Ok(0);
    }
    if leading_ones == ESCAPE {
        return Ok(r.read_bits(64)? as i64);
    }
    let width = BUCKETS[leading_ones as usize - 1];
    Ok(sign_extend(r.read_bits(width)?, width))
}

/// The width `write` would emit, without emitting it. Choosing a scaling exponent means
/// pricing the same block eighteen ways, and running the real writer that many times to throw
/// away seventeen of the results is most of what encoding would cost.
pub fn cost(dod: i64) -> u32 {
    if dod == 0 {
        return 1;
    }
    for (index, &width) in BUCKETS.iter().enumerate() {
        if fits_in(dod, width) {
            return index as u32 + 2 + width;
        }
    }
    ESCAPE + 64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bucket_boundary_survives_a_roundtrip() {
        let mut values = alloc::vec![0i64, 1, -1, i64::MAX, i64::MIN, 86_400, -86_400];
        for width in [7u32, 9, 12, 16, 20, 24, 32, 40] {
            let limit = 1i64 << (width - 1);
            values.extend([limit - 1, limit, -limit, -limit - 1]);
        }
        let mut w = BitWriter::new();
        for &v in &values {
            write(&mut w, v);
        }
        let buf = w.finish();
        let mut r = BitReader::new(&buf);
        for &v in &values {
            assert_eq!(read(&mut r).unwrap(), v);
        }
    }

    /// The estimate exists to be compared against other estimates, but it is only worth
    /// anything if it agrees with the writer it is standing in for, including after someone
    /// edits the bucket table.
    #[test]
    fn the_estimate_matches_what_the_writer_emits() {
        let mut values = alloc::vec![0i64, 1, -1, i64::MAX, i64::MIN];
        for width in [7u32, 9, 12, 16, 20, 24, 32, 40] {
            let limit = 1i64 << (width - 1);
            values.extend([limit - 1, limit, -limit, -limit - 1]);
        }
        for value in values {
            let mut w = BitWriter::new();
            write(&mut w, value);
            assert_eq!(w.bit_len(), cost(value) as usize, "dod {value}");
        }
    }

    #[test]
    fn a_steady_interval_costs_one_bit_per_point() {
        let mut w = BitWriter::new();
        for _ in 0..1000 {
            write(&mut w, 0);
        }
        assert_eq!(w.bit_len(), 1000);
    }
}
