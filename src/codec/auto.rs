//! Encodes with every codec and keeps the smallest block. Blocks are self-describing, so the
//! choice costs nothing at decode time and needs no tag of its own.

use alloc::vec::Vec;

use crate::codec::{Alp, Chimp, Chimp128, Codec, Decimal, Elf, Gorilla, Packed};
use crate::error::Result;
use crate::Point;

pub struct Auto;

impl Codec for Auto {
    fn name(&self) -> &'static str {
        "auto"
    }

    fn encode(&self, points: &[Point]) -> Result<Vec<u8>> {
        let mut best = Gorilla.encode(points)?;
        for codec in [
            &Decimal as &dyn Codec,
            &Chimp,
            &Chimp128,
            &Elf,
            &Alp,
            &Packed,
        ] {
            let candidate = codec.encode(points)?;
            if candidate.len() < best.len() {
                best = candidate;
            }
        }
        Ok(best)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode as decode_block, TAG_DECIMAL};
    use crate::Error;

    #[test]
    fn picks_decimal_for_tenths() {
        let points: Vec<Point> = (0..500)
            .map(|i| Point::new(1_700_000_000 + i * 3600, 8.0 + (i % 40) as f64 / 10.0))
            .collect();
        let block = Auto.encode(&points).unwrap();
        assert_eq!(block[0], TAG_DECIMAL);
        assert_eq!(decode_block(&block).unwrap(), points);
    }

    #[test]
    fn picks_an_xor_codec_when_no_decimal_scale_exists() {
        let mut value = 1.0f64;
        let points: Vec<Point> = (0..500)
            .map(|i| {
                value = f64::from_bits(value.to_bits() + 1);
                Point::new(1_700_000_000 + i * 60, value)
            })
            .collect();
        let block = Auto.encode(&points).unwrap();
        assert_ne!(block[0], TAG_DECIMAL);
        assert_eq!(decode_block(&block).unwrap(), points);
    }

    #[test]
    fn never_loses_to_any_single_codec() {
        let cases: [Vec<Point>; 3] = [
            (0..300)
                .map(|i| Point::new(i * 3600, (i % 360) as f64))
                .collect(),
            (0..300)
                .map(|i| Point::new(i * 3600, 8.0 + (i % 40) as f64 / 10.0))
                .collect(),
            (0..300)
                .map(|i| Point::new(i * 3600, (i as f64).sqrt()))
                .collect(),
        ];
        for points in cases {
            let auto = Auto.encode(&points).unwrap().len();
            for codec in [
                &Gorilla as &dyn Codec,
                &Decimal,
                &Chimp,
                &Chimp128,
                &Elf,
                &Alp,
                &Packed,
            ] {
                let other = codec.encode(&points).unwrap().len();
                assert!(
                    auto <= other,
                    "auto {auto} lost to {} {other}",
                    codec.name()
                );
            }
        }
    }

    /// Whichever codec wins, the tag it left behind has to be one `decode` knows. Asserting a
    /// numeric ceiling here instead would just have to be raised every time a codec is added,
    /// which is not what the test is for.
    #[test]
    fn tag_dispatch_survives_the_choice() {
        let points: Vec<Point> = (0..200)
            .map(|i| Point::new(i * 3600, ((i * 7) % 13) as f64 * 0.125))
            .collect();
        let block = Auto.encode(&points).unwrap();
        assert_ne!(
            decode_block(&block),
            Err(Error::UnknownEncoding(block[0])),
            "auto chose tag {} and decode does not know it",
            block[0]
        );
        assert_eq!(decode_block(&block).unwrap(), points);
    }
}
