# Block formats

Every block is one byte of tag followed by a big-endian bit stream. Bits are written most
significant first, and the final byte is zero-padded. A decoder never needs the padding
length because the point count tells it when to stop.

| tag | codec |
|---|---|
| `0x00` | Gorilla |
| `0x01` | Decimal scaling |
| `0x02` | Chimp |
| `0x03` | Chimp128 |
| `0x04` | Elf |
| `0x05` | ALP |
| `0x06` | Packed |

The tag is what lets a stored block be decoded without recording which codec produced it. It is
also what makes two things possible without any format of their own: the decimal and ALP codecs
emit a Gorilla block when scaling would lose precision, and `Auto` encodes with all six and
keeps the smallest, whichever that turns out to be.

## Shared: delta-of-delta

Timestamps use delta-of-delta in all three codecs, and the decimal codec uses the same
encoding for its scaled integer values. Given a value `v[n]`, the encoder stores
`(v[n] - v[n-1]) - (v[n-1] - v[n-2])`, with both previous deltas taken as zero at the start of
a block. That start condition removes the special case the original paper needs for the
second point, at a cost of one wide bucket per block.

| prefix | payload | range |
|---|---|---|
| `0` | none | exactly 0 |
| `10` | 7 bits, two's complement | -64 to 63 |
| `110` | 9 bits | -256 to 255 |
| `1110` | 12 bits | -2048 to 2047 |
| `11110` | 16 bits | -32768 to 32767 |
| `111110` | 20 bits | ±2^19 |
| `1111110` | 24 bits | ±2^23 |
| `11111110` | 32 bits | ±2^31 |
| `11111111` | 64 bits | anything |

The ranges are the natural two's complement ones rather than the off-by-one ranges printed in
the Gorilla paper. A steady sampling interval costs exactly one bit per point, and an arbitrary
gap always fits, so a series with missing readings needs no special handling.

The buckets past 12 bits are not in the paper, which escapes straight to 64. That cliff is
expensive for any series whose second derivative routinely needs 13 to 32 bits — river
discharge in whole cubic feet per second lands there on almost every point. Each added bucket
costs one bit of prefix to the values above it and saves up to 48 bits every time one is used;
adding them improved every codec in the benchmark.

## Shared: the header

Every block starts the same way:

```
varint    point count
(codec header, if any)
varint    first timestamp, zigzagged
```

Both are LEB128: seven payload bits per byte, high bit set while more follow. Fixed 32- and
64-bit fields wasted most of their width — a Unix timestamp needs 5 varint groups and a typical
block count 1 or 2 — and that waste is exactly what dominates small blocks. Zigzag maps signed
values so small negatives stay short instead of filling all 64 bits.

The first value has no predecessor, so it is stored whole. XOR codecs need all 64 bits of the
pattern; the decimal codec stores a scaled integer, so it uses a zigzag varint there too.

## Gorilla (`0x00`)

```
varint    point count
--- if count == 0, the block ends here ---
varint    first timestamp, zigzagged
64 bits   first value, raw IEEE-754 bit pattern
--- then, for each remaining point ---
          timestamp delta-of-delta
          value, encoded below
```

Each value is XORed against the previous one:

| prefix | meaning | payload |
|---|---|---|
| `0` | XOR is zero, value unchanged | none |
| `10` | reuse the previous window | `64 - leading - trailing` bits |
| `11` | new window | 5 bits leading, 6 bits width, `width` bits |

The previous window is only reusable when the new value's leading and trailing zero counts are
both at least as large as the stored window's, so the significant bits are guaranteed to fit.

Two details are easy to get wrong:

- The leading-zero count is clamped to 31 because the field is 5 bits. Any zeros beyond that
  travel inside the significant-bit window, which stays lossless but wastes space. This is a
  real limit of the original design, not a shortcut taken here.
- The width field is 6 bits and the range is 1 to 64, so a full 64-bit window is stored as 0.
  A width of zero cannot otherwise occur, because that branch is only reached when the XOR is
  non-zero.

## Decimal scaling (`0x01`)

```
varint    point count
 8 bits   decimal scale s, 0 to 17
--- if count == 0, the block ends here ---
varint    first timestamp, zigzagged
varint    first scaled value, zigzagged
--- then, for each remaining point ---
          timestamp delta-of-delta
          scaled value delta-of-delta
```

The encoder picks the smallest `s` in 0 to 17 such that every value `v` in the block satisfies
all of:

- `v * 10^s` is finite and `|v * 10^s| <= 2^53`, above which an f64 can no longer hold every
  integer
- rounding `v * 10^s` to the nearest integer and dividing back by `10^s` reproduces `v`'s exact
  bit pattern
- `v` is not negative zero, which would come back as positive zero after passing through an
  integer

The second condition is the one that is easy to get wrong. Testing whether `v * 10^s` is an
integer instead — the obvious reading of "exactly representable" — rejects a scale of 2 for
8.55, because `8.55 * 100` is `855.0000000000001` in binary floating point. The search then
climbs to 15, where the products are large enough to have left the range where f64 arithmetic
is exact. Rounding to the nearest integer and checking the round trip accepts 2, which is both
correct and four orders of magnitude smaller to encode.

If no such `s` exists, the encoder emits a Gorilla block instead. One awkward value is enough
to disqualify the whole block, which is the price of the scale being a block-level property.

This is also why the codec is not streaming: it must see every value before it can choose `s`
and write the first byte.

## Chimp (`0x02`)

Same framing as Gorilla, with a different value encoding:

| prefix | meaning | payload |
|---|---|---|
| `00` | XOR is zero | none |
| `01` | enough trailing zeros to store them explicitly | 3 bits leading index, 6 bits width, `width` bits |
| `10` | same leading-zero bucket as the previous value | `64 - leading` bits |
| `11` | new leading-zero bucket | 3 bits leading index, `64 - leading` bits |

Leading-zero counts are rounded down into eight buckets — 0, 8, 12, 16, 18, 20, 22, 24 —
addressed by a 3-bit index, so the count costs three bits instead of Gorilla's five. Rounding
down never overstates the zeros, so no significant bit is ever lost.

The `01` branch is taken when there are more than six trailing zeros, which is the point at
which the nine bits of overhead pay for the window they save. After a `01`, the reference
leading count is invalidated: the window just written is narrower than a `10` would imply, so
the next value must state its bucket explicitly.

## Chimp128 (`0x03`)

Same framing again. The difference is that the XOR reference is no longer always the previous
value but the best of the last 128, so two of the four branches carry a 7-bit reference index:

| prefix | meaning | payload |
|---|---|---|
| `00` | XOR is zero | 7 bits reference |
| `01` | enough trailing zeros to store them explicitly | 7 bits reference, 3 bits leading index, 6 bits width, `width` bits |
| `10` | same leading-zero bucket, previous value as reference | `64 - leading` bits |
| `11` | new leading-zero bucket, previous value as reference | 3 bits leading index, `64 - leading` bits |

Finding the reference is the interesting part. A side table of 2^14 entries maps the low 14
bits of a value to the sequence number that last held a value with those same low bits. Two
values agreeing on their low bits XOR to something with at least 14 trailing zeros, so a
single lookup produces a good candidate in constant time. The candidate is used only when it
is still inside the 128-value window and its XOR has more than 13 trailing zeros — 7 bits to
name the reference plus 6 for the width, which is what it has to earn back.

Both branches carrying a reference index invalidate the stored leading count afterwards,
because neither implies a window the next value could reuse.

The failure mode is worth stating plainly: a whole number has all-zero low mantissa bits, so
every whole number collides on key zero. The lookup then returns the previous value and the
7-bit index buys nothing. This is measurable — it is why wind direction is the one variable in
the benchmark where Chimp128 loses to plain Chimp.

## ALP (`0x05`)

```
varint    point count
 8 bits   scaling exponent e, 0 to 17
--- if count == 0, the block ends here ---
  1 bit   whether a patch list follows
--- if that bit is set ---
varint    patch count
--- then, for each patch ---
varint    points since the previous patch, from the start of the block for the first
64 bits   the original value, raw IEEE-754 bit pattern
--- then ---
varint    first timestamp, zigzagged
varint    first scaled value, zigzagged
--- then, for each remaining point ---
          timestamp delta-of-delta
          scaled value delta-of-delta
```

ALP is the decimal codec with the two restrictions that make it brittle lifted.

The first is that a value which will not scale no longer disqualifies the block. It goes into
the patch list whole, and the main stream repeats the last integer that did scale in its place.
Repeating rather than writing a zero is what keeps the cost to a single zero delta-of-delta: a
patched point in the middle of a run leaves the run's deltas exactly as they were, so the values
around it never widen a bucket to accommodate something they are not going to store anyway.

The second is how the exponent is chosen. The decimal codec takes the smallest exponent that
works for every value, because with no patches it has no other option. ALP takes the exponent
that makes the block smallest, which is a different question once some values can be patched: a
block of tenths carrying three full-precision readings is far smaller at exponent 1 with three
patches than at the exponent 17 all of them would need, if such an exponent exists at all.

The cost of an exponent is measured on contiguous sample runs — four of 128 points, or the
whole block when it is shorter — rather than on every point. Pricing 467,550 points eighteen
ways to pick one header byte cost seven times the encoding throughput and did not change a
single figure in the benchmark. The runs are contiguous because delta-of-delta charges for the
shape of a run, and sampling every hundredth point would price a series that does not exist.

Two guardrails:

- A block where more than one value in four needs a patch is emitted as Gorilla instead. Past
  that share the patch list is spending 72 bits a point to store what Gorilla stores in fewer.
  No block in the benchmark comes near the threshold — random bit patterns are what land there,
  and the guardrail is what stops this codec from being the worst available choice on them.
- The patch count is behind a flag bit rather than always present. Most blocks have no patches,
  and a varint zero would spend a whole byte saying so; at six-hour blocks that byte was most of
  what separated this codec from the decimal one.

The paper's departures, both from the backend rather than from the algorithm. ALP bitpacks its
scaled integers with frame-of-reference, which is built for SIMD decoding over columnar vectors;
these go through the same delta-of-delta the decimal codec uses. Measured on the benchmark data,
values only, delta-of-delta spends 8.744 bits a point against the 7.737 frame-of-reference
bitpacking over 1024-value vectors would spend — real, and about one bit a point.

That backend is also why the paper's second scaling axis is missing. ALP writes the scale as an
exponent and a factor, `10^e / 10^f`, because dividing the integers down narrows the
frame-of-reference window. Delta-of-delta prices differences rather than magnitudes, so every
pair `(e, f)` reduces here to the single exponent `e - f` that the search already tries.

## Packed (`0x06`)

```
varint    point count
 8 bits   scaling exponent e, 0 to 17
--- if count == 0, the block ends here ---
  1 bit   whether a patch list follows
--- if that bit is set, the ALP patch list, unchanged ---
varint    first timestamp, zigzagged
varint    first scaled value, zigzagged
--- then every remaining timestamp, delta-of-delta ---
--- then the scaled value deltas, in vectors of 1024 ---
varint    vector minimum, zigzagged
 7 bits   width w, 0 to 64
w bits    each delta in the vector, minus the minimum
```

ALP's own backend, kept apart from the `alp` codec so the two can be compared. Same exponent
search, same patch list, same fallback; the only difference is what happens to the scaled
integers, which is the point.

Where delta-of-delta is variable-length — one bit for a repeated interval, a prefix and a bucket
for anything else — this is fixed-width. The first differences of the scaled integers are split
into vectors of 1024, each vector's minimum is subtracted so the residuals are non-negative, and
every value in the vector is stored in whatever width the largest residual needs. Nobody pays a
prefix, and a steady series whose deltas need five bits costs five bits a point, where
delta-of-delta pays one bit for the steady values and nine for the rest.

The cost is that the width is a vector-level property. One outlier taxes the other 1023 values,
which is exactly what happens to wind direction: NOAA reports it in whole degrees and a reading
crossing north jumps by 350, so a vector holding one such crossing pays nine bits for every
value in it. That is the one variable where this codec loses to `alp`.

Unlike the layouts above, this one cannot interleave: a vector is packed as a unit, so there is
no point at which one timestamp and one value can be read together. Every timestamp comes first,
then every value. Timestamps stay on delta-of-delta, where a regular cadence costs one bit and
frame-of-reference could not beat it — holding them constant is what makes the comparison
against `alp` a comparison of value encodings and nothing else.

The width field is 7 bits rather than the 6 Gorilla uses, because a width of zero is reachable
here. A vector of identical deltas — a constant series, or a perfectly regular ramp — has a span
of zero and stores no value bits at all, so zero cannot be borrowed to mean 64 the way Gorilla
borrows it.

## Elf (`0x04`)

```
varint    point count
 8 bits   decimal places s, 0 to 17
--- then exactly the Chimp layout, over erased values ---
```

Before encoding, each value has as many low mantissa bits zeroed as survive the round trip:
the widest `w` such that rounding `(v with w low bits cleared)` back to `s` decimal places
reproduces `v`'s exact bit pattern. Zeroing bits widens the trailing-zero runs the XOR encoder
is looking for, and the rounding on the way out puts the original value back.

The search runs from 52 bits downwards and returns the first width that survives, so it always
finds the widest one; leaving the value untouched is the safe fallback. Blocks with no valid
`s` at all are emitted as plain Chimp instead.

Elf sits on Chimp rather than Chimp128 for a measured reason. Chimp128 keys its reference table
on the low 14 mantissa bits, which are exactly the bits erasing sets to zero, so every erased
value collides on key zero, the reference degenerates to the previous value, and the 7-bit index
is paid for nothing. Layered on Chimp128 the combination is measurably worse than Chimp128
alone.

## Robustness

Decoders reject impossible windows rather than reading out of range, and every decode path
returns `Err` instead of panicking on truncated or corrupted input. The test suite flips a bit
in every byte position of every block, two thousand times per codec, and asserts that nothing
panics.
