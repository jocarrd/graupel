use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use crate::bits::{unzigzag, zigzag, BitReader, BitWriter};
use crate::error::{Error, Result};
use crate::Point;

mod alp;
mod auto;
mod chimp;
mod chimp128;
mod decimal;
mod dod;
mod elf;
mod gorilla;

pub use alp::Alp;
pub use auto::Auto;
pub use chimp::Chimp;
pub use chimp128::Chimp128;
pub use decimal::Decimal;
pub use elf::Elf;
pub use gorilla::Gorilla;

pub const TAG_GORILLA: u8 = 0;
pub const TAG_DECIMAL: u8 = 1;
pub const TAG_CHIMP: u8 = 2;
pub const TAG_CHIMP128: u8 = 3;
pub const TAG_ELF: u8 = 4;
pub const TAG_ALP: u8 = 5;

pub trait Codec {
    fn name(&self) -> &'static str;

    fn encode(&self, points: &[Point]) -> Result<Vec<u8>>;
}

/// The leading tag byte names the codec, so a stored block stays readable without tracking
/// that separately.
pub fn decode(block: &[u8]) -> Result<Vec<Point>> {
    let (&tag, body) = block.split_first().ok_or(Error::UnexpectedEnd)?;
    match tag {
        TAG_GORILLA => gorilla::decode(body),
        TAG_DECIMAL => decimal::decode(body),
        TAG_CHIMP => chimp::decode(body),
        TAG_CHIMP128 => chimp128::decode(body),
        TAG_ELF => elf::decode(body),
        TAG_ALP => alp::decode(body),
        other => Err(Error::UnknownEncoding(other)),
    }
}

pub fn all() -> Vec<Box<dyn Codec>> {
    vec![
        Box::new(Gorilla),
        Box::new(Decimal),
        Box::new(Chimp),
        Box::new(Chimp128),
        Box::new(Elf),
        Box::new(Alp),
        Box::new(Auto),
    ]
}

pub(crate) struct Dod {
    prev: i64,
    delta: i64,
}

impl Dod {
    pub(crate) fn new(first: i64) -> Self {
        Dod {
            prev: first,
            delta: 0,
        }
    }

    pub(crate) fn write(&mut self, w: &mut BitWriter, value: i64) {
        let delta = value.wrapping_sub(self.prev);
        dod::write(w, delta.wrapping_sub(self.delta));
        self.prev = value;
        self.delta = delta;
    }

    pub(crate) fn read(&mut self, r: &mut BitReader) -> Result<i64> {
        let delta = self.delta.wrapping_add(dod::read(r)?);
        self.prev = self.prev.wrapping_add(delta);
        self.delta = delta;
        Ok(self.prev)
    }
}

/// Turns a value into the 64 bits an encoder actually stores, and back. Gorilla and the Chimps
/// store the raw bit pattern; the decimal codec stores a scaled integer and Elf a rounded-off
/// version of the original.
pub(crate) trait ValueCoding {
    type Encoder: ValueEncoder;
    type Decoder: ValueDecoder;

    fn pack(&self, value: f64) -> u64;
    fn unpack(&self, bits: u64) -> f64;
    fn encoder(&self, first: u64) -> Self::Encoder;
    fn decoder(&self, first: u64) -> Self::Decoder;

    /// Written after the point count, before any point. Empty for most codecs.
    fn write_header(&self, _w: &mut BitWriter) {}

    /// The first value has no predecessor, so it is stored whole. XOR codecs need all 64 bits
    /// of the pattern; codecs storing a scaled integer can use a varint instead, which is
    /// several bytes shorter for any realistic reading.
    fn write_first(&self, w: &mut BitWriter, bits: u64) {
        w.write_bits(bits, 64);
    }

    fn read_first(&self, r: &mut BitReader) -> Result<u64> {
        r.read_bits(64)
    }
}

pub(crate) trait ValueEncoder {
    fn write(&mut self, w: &mut BitWriter, bits: u64);
}

pub(crate) trait ValueDecoder {
    fn read(&mut self, r: &mut BitReader) -> Result<u64>;
}

/// Every block shares this frame: a tag byte, a point count, an optional codec header, then the
/// first point in full and the rest as deltas. Only the value coding differs.
pub(crate) fn encode_block<C: ValueCoding>(
    tag: u8,
    coding: &C,
    points: &[Point],
) -> Result<Vec<u8>> {
    if points.len() > u32::MAX as usize {
        return Err(Error::TooManyPoints(points.len()));
    }
    let mut w = BitWriter::with_capacity(points.len() * 3 + 16);
    w.write_varint(points.len() as u64);
    coding.write_header(&mut w);

    if let Some(first) = points.first() {
        let bits = coding.pack(first.value);
        w.write_varint(zigzag(first.timestamp));
        coding.write_first(&mut w, bits);
        let mut timestamps = Dod::new(first.timestamp);
        let mut values = coding.encoder(bits);
        for point in &points[1..] {
            timestamps.write(&mut w, point.timestamp);
            values.write(&mut w, coding.pack(point.value));
        }
    }

    let mut block = vec![tag];
    block.extend_from_slice(&w.finish());
    Ok(block)
}

pub(crate) fn decode_block<C: ValueCoding>(
    coding_for: impl FnOnce(&mut BitReader) -> Result<C>,
    body: &[u8],
) -> Result<Vec<Point>> {
    let mut r = BitReader::new(body);
    let count = r.read_varint()? as usize;
    let coding = coding_for(&mut r)?;

    let mut points = Vec::with_capacity(count.min(1 << 16));
    if count == 0 {
        return Ok(points);
    }
    let timestamp = unzigzag(r.read_varint()?);
    let bits = coding.read_first(&mut r)?;
    points.push(Point::new(timestamp, coding.unpack(bits)));

    let mut timestamps = Dod::new(timestamp);
    let mut values = coding.decoder(bits);
    for _ in 1..count {
        let timestamp = timestamps.read(&mut r)?;
        let bits = values.read(&mut r)?;
        points.push(Point::new(timestamp, coding.unpack(bits)));
    }
    Ok(points)
}
