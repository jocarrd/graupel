//! ALP's own backend: frame-of-reference and bitpacking over fixed vectors, rather than the
//! delta-of-delta every other codec here uses.
//!
//! This exists to answer a question the rest of the crate cannot. The `alp` codec takes ALP's
//! two ideas — patched exceptions and an exponent chosen by size — and runs them through
//! delta-of-delta, because that is what suits a series whose consecutive readings are close.
//! The paper does not: it subtracts a per-vector minimum and packs what is left into a fixed
//! width, which is built for SIMD decoding over columnar data. Whether that trade wins on
//! weather station readings is not something either paper answers, because neither measures
//! this kind of data.
//!
//! The two encodings pay for a value in completely different ways, which is why the answer is
//! not obvious. Delta-of-delta is variable-length: a repeated interval costs one bit, and an
//! unusual one costs a prefix plus a bucket. Bitpacking is fixed-width: every value in a vector
//! pays whatever the widest value in that vector needs, so one outlier taxes the other 1023 —
//! but nobody pays a prefix, and a steady series with 5-bit deltas costs exactly 5 bits a
//! point where delta-of-delta would pay 1 bit for the steady ones and 9 for the rest.
//!
//! Two things are held constant so the comparison isolates that trade. Timestamps still go
//! through delta-of-delta, because a regular cadence costs one bit there and frame-of-reference
//! could not beat it. And the exponent search, the patch list and the fallback are the same
//! code `alp` uses, so any difference in the benchmark is the value encoding and nothing else.

use alloc::vec;
use alloc::vec::Vec;

use crate::bits::{unzigzag, zigzag, BitReader, BitWriter};
use crate::codec::alp::{sample_runs, split, MAX_EXCEPTIONS_PER, SAMPLE_RUN};
use crate::codec::decimal::{scaled, MAX_SCALE, POW10};
use crate::codec::{gorilla, Codec, Dod, TAG_PACKED};
use crate::error::{Error, Result};
use crate::Point;

/// The paper's vector length. It is also roughly where the per-vector header stops mattering:
/// a reference and a width cost about eleven bytes, which is a tenth of a bit per point here
/// and a byte and a half per point on a six-hour block.
const VECTOR: usize = 1024;

/// Enough for a width of 0 to 64. Six bits with 0 meaning 64 would do, as Gorilla does it, but
/// a width of zero is reachable here — a vector of identical deltas stores nothing at all —
/// so the two cannot share an encoding.
const WIDTH_BITS: u32 = 7;

const EXCEPTION_BITS: u64 = 64 + 8;

pub struct Packed;

impl Codec for Packed {
    fn name(&self) -> &'static str {
        "packed"
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
            for point in &points[1..] {
                timestamps.write(&mut w, point.timestamp);
            }

            let deltas: Vec<i64> = integers
                .windows(2)
                .map(|pair| pair[1].wrapping_sub(pair[0]))
                .collect();
            for vector in deltas.chunks(VECTOR) {
                let (reference, width) = frame(vector);
                w.write_varint(zigzag(reference));
                w.write_bits(width as u64, WIDTH_BITS);
                for &delta in vector {
                    w.write_bits(delta.wrapping_sub(reference) as u64, width);
                }
            }
        }

        let mut block = vec![TAG_PACKED];
        block.extend_from_slice(&w.finish());
        Ok(block)
    }
}

/// The minimum in the vector and the width every value in it will pay. Subtracting the minimum
/// is what makes the residuals non-negative, so the width is the span rather than the magnitude
/// and a vector of large but close values packs as tightly as a vector of small ones.
fn frame(vector: &[i64]) -> (i64, u32) {
    let reference = *vector.iter().min().expect("chunks are never empty");
    let span = vector
        .iter()
        .map(|&delta| delta.wrapping_sub(reference) as u64)
        .max()
        .expect("chunks are never empty");
    (reference, 64 - span.leading_zeros())
}

fn choose_exponent(points: &[Point]) -> u8 {
    let runs = sample_runs(points);
    let mut best = 0;
    let mut best_bits = u64::MAX;
    for exponent in 0..=MAX_SCALE {
        let bits = runs.iter().map(|run| cost_bits(run, exponent)).sum();
        if bits < best_bits {
            best_bits = bits;
            best = exponent;
        }
    }
    best
}

/// What a sample run would cost at this exponent. A run is shorter than a vector, so this
/// prices it as one vector, which is what the encoder would do to a block of that length.
fn cost_bits(points: &[Point], exponent: u8) -> u64 {
    debug_assert!(points.len() <= SAMPLE_RUN.max(VECTOR));
    let factor = POW10[exponent as usize];
    let mut integers = Vec::with_capacity(points.len());
    let mut exceptions = 0;
    let mut fill = 0i64;
    for point in points {
        match scaled(point.value, factor) {
            Some(integer) => {
                fill = integer;
                integers.push(integer);
            }
            None => {
                exceptions += 1;
                integers.push(fill);
            }
        }
    }
    if integers.len() < 2 {
        return exceptions * EXCEPTION_BITS;
    }

    let deltas: Vec<i64> = integers
        .windows(2)
        .map(|pair| pair[1].wrapping_sub(pair[0]))
        .collect();
    let mut bits = 0;
    for vector in deltas.chunks(VECTOR) {
        let (_, width) = frame(vector);
        bits += width as u64 * vector.len() as u64 + 64 + WIDTH_BITS as u64;
    }
    bits + exceptions * EXCEPTION_BITS
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
            if index >= count as u64 {
                return Err(Error::MalformedBlock);
            }
            exceptions.push((index as usize, f64::from_bits(r.read_bits(64)?)));
        }
    }

    let timestamp = unzigzag(r.read_varint()?);
    let integer = unzigzag(r.read_varint()?);
    let mut timestamps = Vec::with_capacity(count.min(1 << 16));
    timestamps.push(timestamp);

    let mut stream = Dod::new(timestamp);
    for _ in 1..count {
        timestamps.push(stream.read(&mut r)?);
    }

    // Values come after every timestamp because the vectors are packed as a unit, so there is
    // no point at which one value and one timestamp can be read together.
    let mut integers = Vec::with_capacity(count.min(1 << 16));
    integers.push(integer);
    let mut remaining = count - 1;
    let mut previous = integer;
    while remaining > 0 {
        let len = remaining.min(VECTOR);
        let reference = unzigzag(r.read_varint()?);
        let width = r.read_bits(WIDTH_BITS)? as u32;
        if width > 64 {
            return Err(Error::MalformedBlock);
        }
        for _ in 0..len {
            let delta = reference.wrapping_add(r.read_bits(width)? as i64);
            previous = previous.wrapping_add(delta);
            integers.push(previous);
        }
        remaining -= len;
    }

    for (timestamp, integer) in timestamps.into_iter().zip(integers) {
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
    use crate::codec::{decode as decode_block, Alp, TAG_GORILLA};

    fn roundtrip(points: &[Point]) -> Vec<u8> {
        let block = Packed.encode(points).unwrap();
        assert_eq!(decode_block(&block).unwrap(), points);
        block
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
    fn two_points_are_a_vector_of_one_delta() {
        roundtrip(&[Point::new(0, 1.5), Point::new(3600, 1.6)]);
    }

    /// Every vector boundary, so an off-by-one in the chunking shows up rather than hiding in
    /// the middle of a long block.
    #[test]
    fn lengths_around_the_vector_boundary() {
        for count in [1022, 1023, 1024, 1025, 1026, 2047, 2048, 2049] {
            let points = tenths(count);
            assert_eq!(decode_block(&roundtrip(&points)).unwrap(), points);
        }
    }

    /// A vector of identical deltas has a span of zero, so the width is zero and the values
    /// occupy no bits at all — the one case a 6-bit width field could not have expressed.
    #[test]
    fn a_constant_series_stores_no_value_bits() {
        let points: Vec<Point> = (0..2000)
            .map(|i| Point::new(1_700_000_000 + i * 3600, 15.5))
            .collect();
        let block = roundtrip(&points);
        assert_eq!(block[0], TAG_PACKED);
        // What is left is the timestamps, one bit each for a steady cadence, plus the header.
        let bits_per_point = block.len() as f64 * 8.0 / points.len() as f64;
        assert!(
            bits_per_point < 1.2,
            "constant series took {bits_per_point:.2} bits a point, so the values cost something"
        );
    }

    #[test]
    fn special_values_survive_as_patches() {
        for odd in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.0, 1e300] {
            let mut points = tenths(100);
            points[7].value = odd;
            let block = roundtrip(&points);
            assert_eq!(
                block[0], TAG_PACKED,
                "value {odd:?} should have been patched"
            );
        }
    }

    #[test]
    fn a_block_that_will_not_scale_falls_back() {
        let mut value = 1.0f64;
        let points: Vec<Point> = (0..500)
            .map(|i| {
                value = f64::from_bits(value.to_bits() + 1);
                Point::new(1_700_000_000 + i * 60, value)
            })
            .collect();
        assert_eq!(roundtrip(&points)[0], TAG_GORILLA);
    }

    #[test]
    fn extreme_deltas_do_not_overflow_the_width() {
        let points = vec![
            Point::new(0, i64::MIN as f64),
            Point::new(3600, i64::MAX as f64),
            Point::new(7200, 0.0),
        ];
        roundtrip(&points);
    }

    /// The point of the codec: on a steady series every delta is the same handful of bits, and
    /// paying a fixed width beats paying a prefix per value.
    #[test]
    fn beats_delta_of_delta_on_a_steady_series() {
        let points: Vec<Point> = (0..4000)
            .map(|i| {
                let daily = (i as f64 / 24.0 * core::f64::consts::TAU).sin() * 6.0;
                Point::new(
                    1_700_000_000 + i * 3600,
                    ((12.0 + daily) * 10.0).round() / 10.0,
                )
            })
            .collect();
        let packed = Packed.encode(&points).unwrap().len();
        let alp = Alp.encode(&points).unwrap().len();
        assert!(packed < alp, "packed {packed} should beat alp {alp}");
    }

    #[test]
    fn a_width_past_sixty_four_is_an_error_not_a_panic() {
        let mut w = BitWriter::new();
        w.write_varint(2);
        w.write_bits(0, 8);
        w.write_bit(false);
        w.write_varint(zigzag(0));
        w.write_varint(zigzag(0));
        w.write_bits(0, 1); // the timestamp delta-of-delta
        w.write_varint(zigzag(0));
        w.write_bits(127, WIDTH_BITS);
        let mut block = vec![TAG_PACKED];
        block.extend_from_slice(&w.finish());
        assert_eq!(decode_block(&block), Err(Error::MalformedBlock));
    }
}
