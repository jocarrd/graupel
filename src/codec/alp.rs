//! ALP, from "ALP: Adaptive Lossless floating-Point Compression" (SIGMOD 2024).
//!
//! The decimal codec already rescales a block to integers. What it cannot do is tolerate a
//! value that will not rescale: the scale is a block-level property, so one NaN, one negative
//! zero or one reading that arrived at full double precision sends the whole block to Gorilla
//! and the other nine hundred values pay for it. ALP's two ideas are what fix that.
//!
//! The first is exceptions. A value that does not survive the scaling is stored whole in a
//! patch list keyed by position, and the main stream repeats the previous value in its place,
//! which costs one zero delta-of-delta rather than widening the buckets around it.
//!
//! The second is how the scale gets chosen. The decimal codec takes the smallest scale that
//! works for every value, because it has no other option. ALP takes the scale that makes the
//! block smallest, which stops being the same question once some values are allowed to be
//! exceptions: a block of tenths carrying three full-precision readings is far smaller at
//! scale 1 with three patches than at the scale 17 it would take to represent all of them,
//! assuming one exists at all.
//!
//! Two deliberate departures from the paper, both because the backend here is not the paper's.
//! ALP encodes its scaled integers with frame-of-reference and bitpacking, built for SIMD
//! decoding over columnar vectors; these go through the same delta-of-delta the decimal codec
//! uses, which suits a series whose consecutive readings are close. That also makes the
//! paper's second scaling axis redundant. ALP writes the scale as an exponent and a factor,
//! `10^e / 10^f`, because dividing the integers down narrows the frame-of-reference window —
//! but delta-of-delta prices differences rather than magnitudes, and every pair `(e, f)`
//! reduces to the single exponent `e - f` that the search already tries.

use alloc::vec;
use alloc::vec::Vec;

use crate::bits::{unzigzag, zigzag, BitReader, BitWriter};
use crate::codec::decimal::{scaled, MAX_SCALE, POW10};
use crate::codec::{dod, gorilla, Codec, Dod, TAG_ALP};
use crate::error::{Error, Result};
use crate::Point;

/// What one patch costs: the 64-bit pattern, plus the one varint group that covers any gap
/// under 128 points. Gaps wider than that are undercounted, which only ever makes a
/// sparsely-patched exponent look slightly better than it is.
const EXCEPTION_BITS: u64 = 64 + 8;

/// Past one exception in four the patch list is spending 72 bits a point to store what Gorilla
/// stores in fewer, so the block goes there instead. Random bit patterns land here, which is
/// what keeps this codec from being the worst available choice on data it was never meant for.
const MAX_EXCEPTIONS_PER: usize = 4;

/// The exponent search prices the block eighteen ways, so it prices samples rather than every
/// point. The samples are contiguous runs, not a stride: delta-of-delta charges for the shape
/// of a run, and picking every hundredth point would measure a series that does not exist.
const SAMPLE_RUN: usize = 128;
const SAMPLE_RUNS: usize = 4;

pub struct Alp;

impl Codec for Alp {
    fn name(&self) -> &'static str {
        "alp"
    }

    fn encode(&self, points: &[Point]) -> Result<Vec<u8>> {
        if points.len() > u32::MAX as usize {
            return Err(Error::TooManyPoints(points.len()));
        }
        let exponent = choose_exponent(points);
        let (integers, exceptions) = split(points, exponent);

        if exceptions.len() * MAX_EXCEPTIONS_PER > points.len() {
            return gorilla::Gorilla.encode(points);
        }

        let mut w = BitWriter::with_capacity(points.len() * 3 + 16);
        w.write_varint(points.len() as u64);
        w.write_bits(exponent as u64, 8);

        if let Some(first) = points.first() {
            // A flag rather than a plain count, because most blocks have no patches at all and
            // a varint zero would spend a whole byte saying so. At six-hour blocks that byte is
            // most of what separates this codec from the decimal one.
            w.write_bit(!exceptions.is_empty());
            if !exceptions.is_empty() {
                w.write_varint(exceptions.len() as u64);
                let mut previous = 0u64;
                for &(index, bits) in &exceptions {
                    w.write_varint(index - previous);
                    w.write_bits(bits, 64);
                    previous = index;
                }
            }

            w.write_varint(zigzag(first.timestamp));
            w.write_varint(zigzag(integers[0]));
            let mut timestamps = Dod::new(first.timestamp);
            let mut values = Dod::new(integers[0]);
            for (point, &integer) in points[1..].iter().zip(&integers[1..]) {
                timestamps.write(&mut w, point.timestamp);
                values.write(&mut w, integer);
            }
        }

        let mut block = vec![TAG_ALP];
        block.extend_from_slice(&w.finish());
        Ok(block)
    }
}

/// Splits a block into the integer stream and the patches. An exception repeats the last
/// integer that scaled, so it lands as a zero delta rather than as a spike the surrounding
/// values would have to widen their buckets for.
fn split(points: &[Point], exponent: u8) -> (Vec<i64>, Vec<(u64, u64)>) {
    let factor = POW10[exponent as usize];
    let mut integers = Vec::with_capacity(points.len());
    let mut exceptions = Vec::new();
    let mut fill = 0i64;
    for (index, point) in points.iter().enumerate() {
        match scaled(point.value, factor) {
            Some(integer) => {
                fill = integer;
                integers.push(integer);
            }
            None => {
                exceptions.push((index as u64, point.value.to_bits()));
                integers.push(fill);
            }
        }
    }
    (integers, exceptions)
}

fn choose_exponent(points: &[Point]) -> u8 {
    let runs = sample_runs(points);
    let mut best = 0;
    let mut best_bits = u64::MAX;
    for exponent in 0..=MAX_SCALE {
        let bits = runs.iter().map(|run| cost_bits(run, exponent)).sum();
        // Ties go to the smaller exponent, which is the one holding smaller integers.
        if bits < best_bits {
            best_bits = bits;
            best = exponent;
        }
    }
    best
}

fn sample_runs(points: &[Point]) -> Vec<&[Point]> {
    if points.len() <= SAMPLE_RUN * SAMPLE_RUNS {
        return vec![points];
    }
    let stride = points.len() / SAMPLE_RUNS;
    (0..SAMPLE_RUNS)
        .map(|run| {
            let start = run * stride;
            &points[start..(start + SAMPLE_RUN).min(points.len())]
        })
        .collect()
}

/// What this run would cost at this exponent. Mirrors the encoder rather than approximating
/// it: same fill-forward on exceptions, same delta-of-delta state, so two exponents are
/// compared on the encoding they would actually produce.
fn cost_bits(points: &[Point], exponent: u8) -> u64 {
    let factor = POW10[exponent as usize];
    let mut bits = 0;
    let mut exceptions = 0;
    let mut fill = 0i64;
    let mut previous = 0i64;
    let mut delta = 0i64;

    for (index, point) in points.iter().enumerate() {
        let integer = match scaled(point.value, factor) {
            Some(integer) => {
                fill = integer;
                integer
            }
            None => {
                exceptions += 1;
                fill
            }
        };
        if index == 0 {
            bits += varint_bits(zigzag(integer));
        } else {
            let step = integer.wrapping_sub(previous);
            bits += dod::cost(step.wrapping_sub(delta)) as u64;
            delta = step;
        }
        previous = integer;
    }
    bits + exceptions * EXCEPTION_BITS
}

fn varint_bits(value: u64) -> u64 {
    let payload = 64 - u64::from(value.leading_zeros());
    payload.div_ceil(7).max(1) * 8
}

pub(crate) fn decode(body: &[u8]) -> Result<Vec<Point>> {
    let mut r = BitReader::new(body);
    let count = r.read_varint()? as usize;
    let exponent = r.read_bits(8)? as usize;
    if exponent >= POW10.len() {
        return Err(Error::MalformedBlock);
    }
    let factor = POW10[exponent];

    let mut points = Vec::with_capacity(count.min(1 << 16));
    if count == 0 {
        return Ok(points);
    }

    let mut exceptions = Vec::new();
    if r.read_bit()? {
        let exception_count = r.read_varint()? as usize;
        if exception_count > count {
            return Err(Error::MalformedBlock);
        }
        exceptions.reserve(exception_count.min(1 << 16));
        let mut index = 0u64;
        for _ in 0..exception_count {
            index = index
                .checked_add(r.read_varint()?)
                .ok_or(Error::MalformedBlock)?;
            // Checked here so the patch loop below can index without a bounds test per point,
            // and so a corrupted list is an error rather than a silently discarded patch.
            if index >= count as u64 {
                return Err(Error::MalformedBlock);
            }
            exceptions.push((index as usize, f64::from_bits(r.read_bits(64)?)));
        }
    }

    let timestamp = unzigzag(r.read_varint()?);
    let integer = unzigzag(r.read_varint()?);
    points.push(Point::new(timestamp, integer as f64 / factor));

    let mut timestamps = Dod::new(timestamp);
    let mut values = Dod::new(integer);
    for _ in 1..count {
        let timestamp = timestamps.read(&mut r)?;
        let integer = values.read(&mut r)?;
        points.push(Point::new(timestamp, integer as f64 / factor));
    }

    for (index, value) in exceptions {
        points[index].value = value;
    }
    Ok(points)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode as decode_block, Decimal, Gorilla, TAG_GORILLA};

    fn roundtrip(points: &[Point]) -> Vec<u8> {
        let block = Alp.encode(points).unwrap();
        assert_eq!(decode_block(&block).unwrap(), points);
        block
    }

    /// Reads the exponent back out of a block rather than assuming where it landed, since the
    /// varint count ahead of it has no fixed width.
    fn exponent_of(block: &[u8]) -> u8 {
        let mut r = BitReader::new(&block[1..]);
        r.read_varint().unwrap();
        r.read_bits(8).unwrap() as u8
    }

    fn tenths(count: i64) -> Vec<Point> {
        (0..count)
            .map(|i| Point::new(1_700_000_000 + i * 3600, 8.0 + (i % 30) as f64 / 10.0))
            .collect()
    }

    #[test]
    fn empty_and_single_point_blocks() {
        roundtrip(&[]);
        roundtrip(&[Point::new(1_700_000_000, 21.5)]);
    }

    #[test]
    fn tenths_of_a_degree_pick_exponent_one() {
        let block = roundtrip(&tenths(200));
        assert_eq!(block[0], TAG_ALP);
        assert_eq!(exponent_of(&block), 1);
    }

    /// The reason this codec exists. The decimal codec has a test asserting that one awkward
    /// value drags the whole block to Gorilla; this is that same block.
    #[test]
    fn one_awkward_value_becomes_a_patch_instead_of_sinking_the_block() {
        let mut points = tenths(100);
        points[50].value = std::f64::consts::PI;
        let block = roundtrip(&points);

        assert_eq!(block[0], TAG_ALP);
        assert_eq!(exponent_of(&block), 1);
        assert_eq!(Decimal.encode(&points).unwrap()[0], TAG_GORILLA);
        assert!(
            block.len() < Decimal.encode(&points).unwrap().len(),
            "alp {} bytes should beat the gorilla fallback {} bytes",
            block.len(),
            Decimal.encode(&points).unwrap().len()
        );
    }

    #[test]
    fn special_values_survive_as_patches() {
        for odd in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.0, 1e300] {
            let mut points = tenths(100);
            points[7].value = odd;
            let block = roundtrip(&points);
            assert_eq!(block[0], TAG_ALP, "value {odd:?} should have been patched");
        }
    }

    /// A patch at the first point has no earlier integer to repeat, and the first integer is
    /// the one written whole ahead of the delta stream.
    #[test]
    fn a_patch_on_the_first_point_roundtrips() {
        let mut points = tenths(100);
        points[0].value = f64::NAN;
        assert_eq!(roundtrip(&points)[0], TAG_ALP);
    }

    #[test]
    fn adjacent_patches_roundtrip() {
        let mut points = tenths(100);
        for point in &mut points[40..44] {
            point.value = std::f64::consts::E;
        }
        assert_eq!(roundtrip(&points)[0], TAG_ALP);
    }

    #[test]
    fn a_block_that_will_not_scale_falls_back_rather_than_patching_every_point() {
        let mut value = 1.0f64;
        let points: Vec<Point> = (0..500)
            .map(|i| {
                value = f64::from_bits(value.to_bits() + 1);
                Point::new(1_700_000_000 + i * 60, value)
            })
            .collect();
        let block = roundtrip(&points);
        assert_eq!(block[0], TAG_GORILLA);
        assert_eq!(block.len(), Gorilla.encode(&points).unwrap().len());
    }

    /// Choosing by size rather than by the first scale that fits: every value here is
    /// representable at exponent 3, but the readings are whole numbers apart from two, and
    /// paying three decimal places on all five hundred to avoid two patches is the trade the
    /// decimal codec has no way to refuse.
    #[test]
    fn prefers_patching_two_values_over_scaling_five_hundred() {
        let mut points: Vec<Point> = (0..500)
            .map(|i| Point::new(1_700_000_000 + i * 3600, (1000 + i % 7) as f64))
            .collect();
        points[100].value = 1000.125;
        points[300].value = 1002.375;

        let block = roundtrip(&points);
        assert_eq!(exponent_of(&block), 0);
        assert!(
            block.len() < Decimal.encode(&points).unwrap().len(),
            "alp {} bytes should beat decimal at scale 3",
            block.len()
        );
    }

    #[test]
    fn a_patch_index_past_the_end_is_an_error_not_a_panic() {
        let mut w = BitWriter::new();
        w.write_varint(4);
        w.write_bits(0, 8);
        w.write_bit(true);
        w.write_varint(1);
        w.write_varint(9); // only four points exist
        w.write_bits(0, 64);
        let mut block = vec![TAG_ALP];
        block.extend_from_slice(&w.finish());
        assert_eq!(decode_block(&block), Err(Error::MalformedBlock));
    }

    #[test]
    fn an_exponent_past_the_table_is_an_error_not_a_panic() {
        let mut w = BitWriter::new();
        w.write_varint(1);
        w.write_bits(200, 8);
        let mut block = vec![TAG_ALP];
        block.extend_from_slice(&w.finish());
        assert_eq!(decode_block(&block), Err(Error::MalformedBlock));
    }

    #[test]
    fn beats_gorilla_on_station_shaped_data() {
        let points: Vec<Point> = (0..2000)
            .map(|i| {
                let daily = (i as f64 / 24.0 * std::f64::consts::TAU).sin() * 6.0;
                Point::new(
                    1_700_000_000 + i * 3600,
                    ((12.0 + daily) * 10.0).round() / 10.0,
                )
            })
            .collect();
        let alp = Alp.encode(&points).unwrap().len();
        let gorilla = Gorilla.encode(&points).unwrap().len();
        assert!(
            alp < gorilla,
            "alp {alp} bytes should beat gorilla {gorilla}"
        );
    }
}
