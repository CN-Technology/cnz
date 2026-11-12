# CNz archive format — specification v1.0 (draft)

*A French version of this document is available in [SPEC.fr.md](SPEC.fr.md).*

Extension: `.cnz` — Proposed MIME type: `application/x-cnz`

All fixed-size integers are **little-endian**.
Structure checksums are **CRC-32C** (Castagnoli).
Content hashes are **BLAKE3** truncated to their first 16 bytes.

## Overview

```
+-----------------------+
| Header (16 bytes)     |
+-----------------------+
| Parameter block       |  only if the archive is encrypted
+-----------------------+
| Data block 0          |  block header (32 bytes) + payload
| Data block 1          |
| ...                   |
+-----------------------+
| Index block           |  main index
| Index block           |  backup copy (only without recovery data)
| Parameter block       |  backup copy (encrypted archive)
+-----------------------+  <- end of the protected area
| Recovery slices       |  optional
| Recovery table        |
| Recovery table        |  backup copy
+-----------------------+
| Footer (48 bytes)     |
+-----------------------+
```

An archive is read from the end: the footer gives the position of the
index, the index describes every file and says in which blocks their data
lies.

## Header (16 bytes)

| Offset | Size | Contents |
|---|---|---|
| 0 | 8 | signature `89 43 4E 5A 0D 0A 1A 0A` (`\x89CNZ\r\n\x1a\n`) |
| 8 | 1 | major version (1) |
| 9 | 1 | minor version (0) |
| 10 | 2 | flags: bit 0 = encrypted archive; the other bits are reserved (0) |
| 12 | 4 | CRC-32C of bytes 0 to 11 |

As with PNG, the `0x89` byte and the `\r\n \x1a \n` sequence make it
possible to detect a file damaged by a text-mode transfer.

A reader **must refuse** a major version it does not know, as well as a
non-zero reserved flag, and **may read** a newer minor version
(compatible additions).

## Block

| Offset | Size | Contents |
|---|---|---|
| 0 | 4 | signature `CNZB` |
| 4 | 1 | type: 0 = data, 1 = index, 2 = encryption parameters |
| 5 | 1 | method: 0 = stored as is, 1 = zstd, 2 = zstd with built-in dictionary no. 1, 3 = CNz context-mixing engine |
| 6 | 2 | reserved (0) |
| 8 | 8 | decompressed size |
| 16 | 8 | size of the (stored) payload |
| 24 | 4 | CRC-32C of the stored payload |
| 28 | 4 | CRC-32C of bytes 0 to 27 |

The payload immediately follows the header. Each block is **independent**:
a damaged block only affects the files whose data lies in it. The
decompressed size of a block cannot exceed 1 GiB.

**Built-in dictionary no. 1** (method 2): zstd dictionary of 1,048,576
bytes, zstd identifier 0x577D8FC4, BLAKE3 starting with
`2c9511b957d24c3fa275b00176881462`. It is **frozen**: the reference file
is `dictionnaire/cnz-1.dict.zst` (compressed with zstd); every reader
must embed it identically. The payload is a zstd frame compressed with
this dictionary. The writer tries it on blocks of at most 4 MiB and keeps
the smallest result.

**Context-mixing engine** (method 3, "maximum" mode): see the section
"Context-mixing engine, model no. 1" below.

A data block contains, end to end, chunks of files ("extents"). A small
file fits in a single block, a large file may span several consecutive
blocks.

## Encryption

An encrypted archive (bit 0 of the header) contains a **parameter block**,
stored in clear, right after the header and as a backup copy right before
the footer:

```
varint  version (1)
varint  key derivation: 1 = Argon2id (version 0x13)
varint  memory in KiB (8 to 1,048,576)
varint  number of passes (1 to 16)
varint  parallelism (1 to 64)
varint  salt length (16), then the salt (random)
varint  cipher: 1 = XChaCha20-Poly1305
32      check value
```

From the password (UTF-8) and the salt, Argon2id produces a 32-byte
master key. Two subkeys are derived from it with `BLAKE3 derive_key`
(the context strings below are part of the format and are written
exactly as shown, in French):

- `"CNZ v1 chiffrement des blocs"`: the XChaCha20-Poly1305 key;
- `"CNZ v1 verification du mot de passe"`: the check value, which makes
  it possible to refuse a wrong password without decrypting anything.

Each data and index block is first compressed, then encrypted. Its
payload becomes:

```
24      random nonce
n       encrypted data
16      Poly1305 seal
```

The seal also covers these 18 bytes (associated data):
type (1), method (1), decompressed size (8), number (8) — number of the
data block (0, 1, 2…), or for an index block its own position in the
archive with the high bit set to 1 (each copy is sealed separately). A
block that is modified, moved or swapped with another is therefore
refused.

Since the index is encrypted, **file names cannot be read** without the
password. What remains visible: the number of blocks and their size.

The CRC-32C of the blocks covers the encrypted payload: the integrity of
an encrypted archive can be checked (and repaired) without the password.

## Recovery data

Optional (3 % by default). It makes it possible to **rebuild** the
damaged parts of the **protected area**: everything that precedes the
recovery slices (header, parameters, blocks, index).

**Slicing.** The protected area, of length P, is cut into slices of S
bytes: S = max(4096, ⌈P / 65536⌉) rounded up to the next multiple of
4096. The last, shorter slice is padded with zeros for the computation.

**Interleaved groups.** There are G groups: G = ⌈n / k_max⌉, where n is
the number of slices and k_max = min(4096, ⌊64 MiB / S⌋). Slice i belongs
to group i mod G. A contiguous damaged area is thus spread over all the
groups.

**Reed-Solomon codes.** For a group of k slices, r = max(1, ⌈k × ‰ /
1000⌉) recovery slices are computed with the Reed-Solomon code over
GF(2^16) of Lin, Han and Chung (the "Leopard" algorithm, as implemented
by the `reed-solomon-simd` library). Any r lost slices of the group can
be rebuilt.

**Layout.** The recovery slices follow the protected area, group by group
(group 0 first). Then come the **recovery table** (block of type 3,
stored as is) and its copy:

```
varint  version (1)
varint  length of the protected area P
varint  slice size S
varint  number of groups G
varint  redundancy per thousand
varint  position of the first recovery slice
n × 4   CRC-32C of each slice of the protected area (actual length)
R × 4   CRC-32C of each recovery slice
```

**Repair.** The reader finds the table (through the footer, otherwise by
searching for the `CNZB` signature in the whole file), spots the slices
whose CRC does not match, and rebuilds each group that has not lost more
slices than it has healthy recovery slices. The repair works on the
stored bytes: an encrypted archive can be repaired without the password.

## Footer (48 bytes)

| Offset | Size | Contents |
|---|---|---|
| 0 | 4 | signature `CNZE` |
| 4 | 4 | reserved (0) |
| 8 | 8 | position of the main index |
| 16 | 8 | position of the index backup copy (0: none) |
| 24 | 8 | position of the recovery table (0: none) |
| 32 | 8 | total size of the archive |
| 40 | 4 | CRC-32C of bytes 0 to 39 |
| 44 | 4 | end signature `CNZ!` |

## Index backup copy

The index is written twice only if the archive has **no** recovery data.
With it, the index can be repaired like the rest of the protected area,
and the copy cost about 1.5 % of the archive.

## Index (version journal)

The index is a **journal**: each compression or addition writes an index
**layer** that holds only its novelties and refers to the previous
layer. Each layer is a **version** of the archive. The footer points to
the last layer (and its backup copy).

A layer is the (decompressed) payload of a block of type 1. Integers are
coded as **varints** (unsigned LEB128: 7 bits per byte, high bit = "more
to come"). Signed integers go through zigzag coding.

```
varint  index format version (2)
varint  archive version number (1, 2, 3…)
varint  write date, seconds since 1970
varint  position of the previous layer (0 for version 1)
varint  position of the backup copy of the previous layer (0: none)
varint  number of the first block added by this layer
varint  number of added blocks N
N ×     varint position of the block in the archive
varint  number of removed paths R
R ×     varint length, then the path
varint  number of added entries E
E ×     entry
```

**Reading a version V**: start from the last layer, walk back up the
chain to version 1 (numbers must decrease one by one), then apply layers
1 to V in order: add their blocks (the first number of each layer must
follow the last block already known), remove the listed paths, add the
entries. Data blocks are numbered globally, from 0 to the last block of
the last layer.

**Addition**: the recovery data and the footer are removed, the new
blocks are written, then a new layer that refers to the previous one.
Old layers stay valid: an earlier version can still be recovered, and an
interrupted addition leaves the archive readable in its previous state.

**Compacting**: rewriting of the archive with a single layer (version 1)
holding the state of the last version. Fully useful blocks are copied
without recompression, mixed blocks are recompressed, stale blocks
disappear.

Entry:

```
u8      type: 0 = file, 1 = folder, 2 = symbolic link,
        3 = rebuilt file
varint  path length, then the path (bytes, "/" separator)
varint  permissions (bits 0o7777)
varint  modification date, seconds since 1970 (zigzag)
varint  nanoseconds
-- file (types 0 and 3) --
varint  file size
-- rebuilt file only (type 3) --
16      BLAKE3 of the original file, truncated
varint  size of the transformed form (at most 1 GiB)
-- file (types 0 and 3) --
varint  number of extents X
X ×     varint block number, varint position in the block, varint length,
        16 bytes: BLAKE3 of the chunk, truncated
-- symbolic link --
varint  target length, then the target
-- for every entry --
varint  number of optional fields F
F ×     varint tag, varint length, then the value
```

The extents of a file are given in file order; the sum of their lengths
equals the size of the file (type 0) or that of its transformed form
(type 3). Each chunk carries its own hash: the writer computes it during
the single read of the file, the reader checks each chunk before writing
it.

### Rebuilt file (type 3)

A PDF, a ZIP (Office and OpenDocument documents included), a .gz file or
a PHP archive (PHAR) contains Deflate-compressed streams (RFC 1951). When
each of these streams can be redone **byte for byte** by zlib's
algorithm, the writer may store the file in a **transformed form**, where
the streams are replaced by their decompressed contents, which compress
much better.

A reader that does not know type 3 refuses the index ("unknown entry
type"): it cannot mistakenly extract the transformed form instead of the
file.

Transformed form (the bytes described by the extents):

```
varint  version (1)
varint  number of segments S
S ×     varint type: 0 = copy, 1 = Deflate stream, 2 = Deflate stream in slices,
        3 = predicted Deflate stream, 4 = recompressed JPEG photo, 5 = PNG image,
        6 = PNG pixels (zlib + image model), 7 = PNG pixels (predicted +
        image model), 8 = embedded file, 9 = Deflate stream (zlib) with
        transformed contents, 10 = predicted Deflate stream with transformed
        contents, 11 = TrueType font, 12 = WAV sound, 13 = program
        -- copy --
        varint  length L
        -- Deflate stream (types 1 and 2) --
        varint  length of the decompressed contents L
        u8      level (1 to 9)
        u8      memory, memLevel (1 to 9)
        u8      window, windowBits (9 to 15)
        -- in slices only (type 2) --
        varint  slice size T (4096 to 16,777,216)
        -- predicted stream (type 3) --
        varint  length of the decompressed contents L (at most 256 MiB)
        varint  length of the corrections C
        -- JPEG photo (4), PNG image (5), font (11), sound (12), program (13) --
        varint  length L of the segment data
        -- PNG pixels, zlib (type 6) --
        varint  length of the decompressed contents, u8 level, u8 memory,
        u8 window, varint slice size (0: in one go), 9 image bytes
        (width u32, height u32 little-endian, colour type u8),
        varint length L of the coded pixels
        -- PNG pixels, predicted (type 7) --
        varint  length of the decompressed contents, varint length of the
        corrections C, 9 image bytes, varint length L of the coded pixels
        -- embedded file (type 8) --
        varint  length L of its transformed form, varint its size
        -- stream with transformed contents, zlib (type 9) --
        varint  length of the decompressed contents, u8 level, u8 memory,
        u8 window, varint slice size, varint length L of the transformed
        form of the contents
        -- stream with transformed contents, predicted (type 10) --
        varint  length of the decompressed contents, varint length of the
        corrections C, varint length L of the transformed form of the contents
then, for each segment in order, its L bytes of data (type 3: L bytes of
decompressed contents followed by C bytes of corrections)
```

The original file is the sequence of the segments: a copy gives its bytes
as they are; a Deflate stream gives the **raw** Deflate compression
(without header or checksum) of its data by the zlib 1.3 algorithm
(function `deflateInit2` with that level, memory and window, default
strategy, then `deflate(Z_FINISH)`). The zlib or gzip headers and the
checksums around a stream are copy segments.

**In slices** (type 2), the way LibreOffice compresses large files on
several cores: the data is cut into slices of T bytes (the last one
shorter). Each slice is compressed by a fresh zlib compressor (same
settings), which first receives as a dictionary (`deflateSetDictionary`)
the 2^windowBits bytes preceding the slice (fewer at the start); each
slice is ended by `deflate(Z_SYNC_FLUSH)`, except the last one, ended by
`deflate(Z_FINISH)`. The outputs are put end to end.

The reader **must**: refuse an unknown version, an unknown segment type,
an out-of-range setting, extra or missing data; check that the rebuilt
file has exactly the announced size; check its BLAKE3 (16 bytes) before
writing it. A writer **must** perform the whole rebuild and compare it
with the original file before choosing the transformed form.

**Predicted stream** (type 3), for a stream written by a program other
than zlib: the stream is described by its **tokens** (a literal, or a
repetition of length 3 to 258 at distance 1 to 32,768, RFC 1951) and the
structure of its blocks; a predictor guesses each token from the
decompressed contents, the corrections only record its mistakes. The
stream is then rewritten bit for bit: canonical Huffman codes (RFC 1951,
3.2.2) derived from the lengths as they are written, length 258 coded by
symbol 285 unless stated otherwise.

*Predictor.* 3-byte hash chains: h(i) = ((d[i] << 16 | d[i+1] << 8 |
d[i+2]) × 0x9E3779B1 mod 2^32) >> 16; each position i (i + 3 ≤ L) is
inserted at the head of chain h(i) once processed (all the positions
covered by a token, and those of stored blocks). The **candidates** at
position p are, among the first 4,096 positions of chain h(p) (from the
most recent to the oldest, stopping at the first one more than 32,768
bytes away), those whose common length with p (bounded by min(258,
L − p)) is at least 3. The **best** repetition is the longest (the
closest at equal length); the walk stops as soon as it reaches the
bound. Forecast: a literal if the best is shorter than 3; a literal too
if it is exactly 3 long and the 3-byte rule rules it out (rule 1:
distance beyond 4,096, rule 2: always); in **lazy** mode, a literal too
if the best is shorter than 258 and the best at p + 1 (p being inserted
provisionally) is longer; otherwise the best.

*Corrections* (integers as varints):

```
varint  version (1)
u8      mode (0 to 5): bit 0 = lazy, bits 1-2 = 3-byte rule (0 to 2)
u8, u8  end bits of the stream (value, count)
varint  lengths of the three parts: structure, kinds, details
-- structure --
varint  number of blocks (the last one carries the "last block" bit)
per block: u8 kind (0 = stored, 1 = fixed, 2 = dynamic)
  stored: u8 padding, u8 number of padding bits, u16 LEN, u16 NLEN
  others: varint number of tokens, varint number of lengths 258 coded
          284 + 31, then their ranks (gaps as varints);
          dynamic: varint HLIT (257 to 286), u8 HDIST (1 to 30),
          varint number of code-length code lengths then those lengths
          (u8 each), varint number of written symbols then each symbol
          (u8, 0 to 18) followed for 16, 17, 18 by its extra bits (u8)
-- kinds: one byte per token --
0 = the forecast; 1 = a literal; 2 = a "gap, rank" repetition;
3 = an explicit repetition
-- details, in the order of the kind-2 or kind-3 tokens --
kind 2: varint gap E, varint rank R — length = best − E (at least 3);
        distance = that of the rank-R candidate among those of at least
        this length
kind 3: varint length, varint distance (the repetition must match the
        contents)
```

The contents of stored blocks are taken from the decompressed contents.
The reader **must** refuse corrections that are incomplete, too long or
inconsistent with the contents.

**Recompressed JPEG photo** (type 4), for a sequential JPEG photo (SOF0
or SOF1, 8 bits, Huffman, a single scan of all components, restart
markers included) or a progressive one (SOF2, see below): the segment
covers the whole file.

```
varint  version (1)
varint  length H of the headers (from the start of the file up to the SOS
        scan header included), varint length F of the end (from the
        end-of-image marker), varint length Z
Z       headers then end, compressed by zstd
varint  number of data segments (1 + number of restart markers), then for
        each: u8 value of the fill bits, u8 their count
varint  number of blocks written in an unusual way, then for each:
        varint gap from the previous block number, u8 number of
        superfluous "16 zeros" codes (ZRL) before the end of the block,
        u8 end of block (EOB) written (1) or not (0)
the rest: the coefficients, arithmetic-coded
```

Rebuild: read the headers (Huffman and quantisation tables, dimensions,
sampling, restart interval), decode the coefficients, then rewrite the
image data with the file's tables: DC as a difference from the previous
block of the same component (reset at each restart), AC as (run of
zeros, category), ZRL for 16 zeros, EOB if zeros remain after the last
non-zero coefficient (unless an exception is recorded), byte 0x00 after
each 0xFF, fill bits and RSTn marker at the end of each segment.

*Progressive JPEG* (version 2 of the segment data):

```
varint  version (2)
varint  length H of the headers (up to the first scan header included),
        varint length F of the end, varint number N of scans, then
        (N − 1) × varint length of the bytes preceding each scan after
        the first one (tables, restart interval, header)
varint  length Z, then Z bytes: headers, end and those bytes concatenated,
        compressed by zstd
varint  length I, varint length J, then J bytes: the description of the
        scans (I bytes) compressed by zstd. For each scan: varint number
        of segments, then u8 value and u8 count of the fill bits of each;
        u8 1 if the runs of empty blocks follow libjpeg's rule, otherwise
        0 followed by varint their number and the length of each one
        (varint)
the rest: the coefficients (as for a sequential photo)
```

Rewriting of each scan according to the standard (ITU T.81, G.1.2): DC
first pass (difference of coef >> Al, arithmetic shift), DC refinement
(bit Al), AC first pass (|coef| >> Al with its sign, ZRL, runs of empty
blocks EOBn), AC refinement (deferred correction bits, like libjpeg). A
run is written as soon as it reaches the recorded length; libjpeg's
rule: as long as possible, at most 32,767 blocks, in refinement as soon
as more than 937 correction bits are waiting, and always before a
coefficient, a ZRL, a restart or the end of the scan. The end (F) starts
at the first marker that leads to no new scan (comment, table, end of
image).

*Arithmetic coder* (shared with the PNG image model): LZMA's. Coder:
64-bit `low` = 0, 32-bit `range` = 0xFFFFFFFF, `cache` = 0,
`cache_size` = 1. To code a bit with probability `p` (on 12 bits,
probability of a 0): `bound = (range >> 12) × p`; bit 0: `range = bound`;
bit 1: `low += bound`, `range −= bound`. Then while `range < 2^24`:
`range <<= 8` and shift of `low` (if the 32-bit `low` < 0xFF000000 or if
there is a carry beyond 32 bits: write `cache + carry`, then
`cache_size − 1` bytes 0xFF + carry, `cache` = byte 24 to 31 of `low`;
`cache_size += 1`; `low = (low & 0xFFFFFF) << 8`). At the end, 5 shifts.
Decoder: `range` = 0xFFFFFFFF, `code` = the first 5 bytes (big-endian,
the first one is 0); bit = `code ≥ bound`; beyond the end of the data,
the bytes read are 0 (at most 5). Each probability starts at `p` = 2048
with a counter `n` = 0; after each bit, step `s` = (1, 2, 2, 3, 3, 3, 4,
4, 4, 4, 5, 5)[`n`], then bit 1: `p −= p >> s`, bit 0: `p += (4096 − p)
>> s`, and `n = min(n + 1, 11)`.

*Coefficient model.* Blocks are walked through component by component,
and within each one row by row, over the complete grid (aligned on MCUs;
single component: the grid of its blocks). Two independent sets of
probabilities: component 0, and all the others. `a`, `l`, `d` are the
blocks above, to the left and diagonally (above-left) in the same
component (absent at the edge, counted as zero); coefficients are indexed
in zigzag order `k` (0 = DC); `Z[k]` is the natural position (row `v =
Z[k] / 8`, column `u = Z[k] mod 8`); `bitlen(x)` is the number of bits of
`x` (0 for 0). The quantisation table is that of the component in the
headers (1 everywhere if missing); `deq(b, n)` is the coefficient of `b`
at natural position `n` multiplied by its quantisation step. `B[i][x]` is
the integer DCT basis: `round(4096 × C(i) × cos((2x + 1)iπ/16))`, C(0) =
1/√2 (8 × 8 values, written in `src/jpegmodel.rs`); `div(n, d)` is
division rounded to the nearest (halves away from zero).

1. **Number of non-zero AC coefficients** `N` (0 to 63), on 6 bits from
   the most to the least significant, in a binary tree (node 1, then
   2 × node + bit). Context: `na`, `nl` = counts of the blocks above and
   to the left; both present: `min(bitlen(na), 6) × 7 + min(bitlen(nl),
   6)`; above only: `49 + min(bitlen(na), 6)`; left only: `56 +
   min(bitlen(nl), 6)`; none: 63.
2. **AC coefficients**, `k` from 1 to 63 as long as `R` > 0 non-zeros
   remain to be coded (the others are zero). `g` = position group: (0, 1,
   2, 3, 4, 5, 5, 6, 6, 6, 7 ×5, 8 ×6, 9 ×7, 10 ×8) for `k` = 0, 1, 2… 35,
   then 11; `r = min(bitlen(R), 7)`; neighbouring amplitude `A` =
   (13·|a[k]| + 13·|l[k]| + 6·|d[k]| + 16) / 32 if above and left exist,
   otherwise |a[k]| or |l[k]|, otherwise 0; `m = min(bitlen(A), 11)`; `i`
   = number of coefficients of this block already non-zero to the left
   (`u > 0`, natural position Z[k] − 1) and above (`v > 0`, Z[k] − 8), the
   DC never being one of them. "Non-zero?" with probability no. `((k × 8
   + r) × 12 + m) × 3 + i`, unless `64 − k = R` (necessarily non-zero). A
   non-zero value is coded by the *value* procedure with size table no.
   `(k × 12 + m) × 3 + min(r, 4) / 2`, the bit table of group `g`, and for
   the sign probability no. `g × 9 + (sign(a[k]) + 1) × 3 + (sign(l[k]) +
   1)`; **except** for the first row (`v = 0`, block above present) and
   the first column (`u = 0`, block to the left present): their sign is
   postponed (absolute value for now).
3. **Postponed signs**, in order: first row `u` = 1 to 7 (rank `e` =
   u − 1), then first column `v` = 1 to 7 (`e` = 6 + v). Prediction (row):
   `P = div(Σ_y B[y][7]·deq(a, 8y + u) − Σ_{y≥1} B[y][0]·deq(block, 8y +
   u), B[0][0] × q[k])`; (column): the same with `deq(l, 8v + x)` and
   `deq(block, 8v + x)`, x instead of y. Sign (1 = negative) with
   probability no. `((e × 12 + min(bitlen(|P|), 11)) × 3 + sign(P) + 1) ×
   8 + min(bitlen(|value|), 7)`.
4. **DC**: `U = B[0][0] × q[0]`; above: `X = Σ_y B[y][7]·deq(a, 8y) −
   Σ_{y≥1} B[y][0]·deq(block, 8y)`; left: `Y = Σ_x B[x][7]·deq(l, x) −
   Σ_{x≥1} B[x][0]·deq(block, x)`. Prediction `P` = `div(X + Y, 2U)`
   (both), `div(X, U)` or `div(Y, U)` (only one), 0 otherwise, bounded to
   ±2047; uncertainty `S` = |div(X, U) − div(Y, U)| (both), otherwise 0;
   context `c = min(bitlen(S), 11)`. Difference `DC − P`: "non-zero?"
   (probability no. `c`), then *value* with size table no. `c`, bit table
   no. `c` and for the sign probability no. `sign(P) + 1`. The DC must be
   within [−2048, 2047].

*Value* (non-zero): with `T` the number of probabilities of the size
table (11 for ACs, 12 for the DC): size `t` in unary ("more than 1
bit?", "more than 2?"… with the `t`-th probability, at most `T + 1`);
the sign if it is not postponed; then the `t − 1` bits below the most
significant bit, from the most to the least significant: the first two
with probability no. `min((t − 1) × T + i, T² − 1)` of the bit table
(`i` = bit rank, `T²` probabilities per table), the following ones with a
probability shared by the whole model. An AC value must be less than 1024
in absolute value.

**PNG image** (type 5): the segment covers the whole file. The IDAT
chunks of a PNG, put end to end, form a single zlib stream; the "glued"
file (headers, CRCs and IDAT split removed) is described by its own
transformed form.

```
varint  version (1)
varint  position P of the first IDAT chunk in the file
varint  size V of the glued file
varint  number N of IDAT chunks, then N × varint length of their data
the rest: transformed form of the glued file (it contains no type-5
segment)
```

Rebuild: rebuild the glued file (V bytes), then write its first P bytes,
each chunk (length on 4 big-endian bytes, "IDAT", its data taken in
sequence from the glued file, CRC-32 of the type and the data), then the
rest of the glued file.

**PNG pixels coded by the image model** (types 6 and 7, inside the glued
file of a PNG): like types 1 and 3, but the decompressed contents
(filtered rows) are replaced by their coded form: filter of each row,
then values of each pixel (8 bits, non-interlaced; grey, colour,
transparency, palette) as a difference from a median prediction
(JPEG-LS), arithmetic-coded (same coder as photos); when decoding, the
filters are applied again. **Version 1: the reference implementation
(`src/pngmodel.rs`) is authoritative** for the contexts.

**Embedded files** (types 8, 9 and 10): a photo or a PNG stored inside a
document (ZIP, Office) is described by **its own transformed form**,
whether it is stored as is (type 8: the segment gives back the file) or
Deflate-compressed (types 9 and 10: the transformed form gives back the
decompressed contents, then recompressed as for types 1 and 3). The same
image thus has the same form in every document. An inner transformed
form contains no segment of type 8, 9 or 10 (a single nesting level);
the reader must refuse otherwise.

**TrueType font** (type 11, stage 6): the segment covers the whole file
(font starting with `00 01 00 00` or `true`, with `glyf` and `loca`
tables). Glyphs are stored in separate streams, bytes unchanged; the
`loca` table and the exact bounding boxes are recomputed. An older reader
refuses this segment ("unknown type") and does not extract the file.
Segment data:

```
varint  version (1)
varint  position of loca, varint length of loca, varint long loca (0 or 1,
        from head.indexToLocFormat), varint position of glyf, varint
        length of glyf
varint  length R of the rest, then R bytes: the file without the bytes of
        loca or glyf (both tables removed in place)
12 ×    varint length, then the bytes of the stream:
  1 kinds       u8 per glyph: 0 empty, 1 simple (box recomputed),
                2 simple (box stored), 3 composite
  2 paddings    varint per simple glyph: bytes after its coordinates
  3 contours    i16 (big-endian) per non-empty glyph: number of contours
  4 boxes       8 bytes per kind-2 glyph
  5 end points  2 × contours bytes per simple glyph
  6 instructions u16 length I then I bytes, per simple glyph
  7 flags       flag bytes as they are (repetitions included)
  8 X, 9 Y      coordinate bytes as they are (1 or 2 depending on the flags)
 10 composites  varint per composite glyph: length after the number of contours
 11 composites  those bytes as they are
 12 padding bytes, then the end of glyf after the last glyph
```

Glyphs are rebuilt in order, each one right after the previous one; loca
receives the position of each one (divided by 2 if loca is short), then
the end of the last one. Reading the flags: a flag byte, followed by a
repeat byte if bit 3 is set, until the number of points is reached (last
end point + 1); extra flags are ignored for the coordinates. Recomputed
box: minimum X, minimum Y, maximum X, maximum Y of the points (cumulated
coordinates), as big-endian i16.

**WAV sound** (type 12, stage 6): the segment covers the whole file (RIFF
WAVE, integer PCM format 1 or extended 0xFFFE with PCM subformat, 1 to 64
channels, 1 to 32 bits per sample rounded up to the byte). Data:

```
varint  version (1)
varint  position P of the samples, varint length L (whole frames)
varint  channels C, u8 bytes per sample W (1 to 4), u8 order (1 or 2)
varint  P, then the first P bytes of the file
L bytes: the differences, by byte planes (byte 0 of all differences, then
        byte 1…), differences in the order of the file's samples
then    the end of the file (after the L bytes of samples)
```

Computations modulo 2^n (n = 8W). For each channel, x₁ and x₂ are the two
previous samples of the channel (0 at the start), read as signed n-bit
integers; prediction p = x₁ (order 1) or 2x₁ − x₂ (order 2), brought back
modulo 2^n. Difference e = (x − p) mod 2^n read as signed, stored in
zigzag ((e << 1) xor (e >> 63), on n bits). The writer chooses the order
whose sum of zigzag differences is the smallest.

**Program** (type 13, stage 6): the segment covers the whole file. Data:

```
varint  version (1)
u8      architecture: 1 = x86 / x86-64, 2 = ARM64
varint  number of zones Z (at most 256), then Z times: varint start,
        varint length (zones sorted, non-overlapping, inside the file)
then    the file, with filtered zones
```

The zones are the code sections (64-bit ELF, PE or Mach-O); the reader
does not have to find them again, it applies the inverse filter to each
zone. Positions i are those of the file.

- x86: from i = start while i + 5 ≤ end: if byte i is E8 or E9, then if
  byte i + 4 is 00 or FF, the value v (little-endian i32 at i + 1)
  becomes v + (i + 5) when coding, v − (i + 5) when decoding (32-bit
  computation), brought back to 25 signed bits (high bytes 00 or FF); in
  every case i advances by 5. Otherwise i advances by 1.
- ARM64: for each 4-byte word (little-endian u32) at i = start, start +
  4…: BL call (top 6 bits 0x25): the low 26 bits receive (word + (i >>
  2)) when coding, (word − (i >> 2)) when decoding, modulo 2^26; ADRP
  (word & 0x9F000000 = 0x90000000): s = immlo | immhi << 2 (21 bits); if
  s is within [−2^17, 2^17) (read on 21 bits), d = s ± (i >> 12) then
  immlo = d & 3, immhi = bits 2 to 17 of d, bits 18 to 20 = bit 17 of d
  (like xz's ARM64 filter); otherwise the word is unchanged.

zlib's output depends on its exact algorithm: CNz ships zlib 1.3.2,
pinned version, and a test checks at each build that it still produces
the same bytes (reference confirmed with an independent zlib, 1.3.1).

### Optional fields

A reader **must ignore** the tags it does not know: this is what allows
information to be added without breaking existing versions.

| Tag | Contents |
|---|---|
| 1 | user ID (varint) |
| 2 | group ID (varint) |
| 3 | user name (UTF-8) — takes precedence over the ID when restoring |
| 4 | group name (UTF-8) — takes precedence over the ID when restoring |
| 5 | creation date: seconds (zigzag), nanoseconds |
| 6 | extended attribute: varint name length, name, then value |
| 7 | Windows attributes (varint, GetFileAttributes value) |

The owner is only restored on request or as administrator.

Entries are stored so that a folder appears before its contents.

## Safety rules for extraction

A reader **must** refuse any path:
- empty, or starting with `/`;
- containing an empty component, `.` or `..`;
- containing a null byte.

A reader **must** check the BLAKE3 of each extracted chunk and report any
file whose contents do not match.

### File names depending on the system

Archive paths are bytes. On Unix these are the bytes of the name as they
are; on Windows, the UTF-8 of the name. The separator is always "/",
including in the target of a symbolic link.

When extracting on Windows, each component is made acceptable:
characters `< > : " / \ | ? *` and control characters replaced by `_`,
trailing dot or space replaced by `_`, reserved name (`CON`, `PRN`,
`AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`, whatever the extension)
prefixed with `_`. Since the backslash is a separator on Windows, this
replacement is also a safety rule.

If the destination disk mixes up upper and lower case (Windows, macOS,
FAT/exFAT — to be checked on the disk itself), two names that are equal
apart from case and Unicode normalisation (NFC) are told apart by
numbering the second one: `photo.jpg` → `photo (2).jpg`. A renamed folder
takes its contents along.

Any renaming is reported to the user.

## Recovery

0. If the archive contains recovery data, repair first.
1. If the main index is unreadable, use the backup copy. If there is none
   (the archive has recovery data), refuse to open it and offer the
   repair: never fall back on another index, which could be that of an
   older version.
2. If the footer is unreadable, walk through the archive block by block
   (jumping from `CNZB` signature to `CNZB` signature) and keep the last
   valid index.
3. A damaged data block does not prevent extracting the other files.

## Context-mixing engine, model no. 1 (block method 3)

The code of `src/cm.rs` is the reference; the `modele_fige` test checks
the hash of a typical compression. All computations are **integer**;
`>>` is an arithmetic shift (towards −∞), `/` an integer division
truncated towards 0, computations "on 32 bits" are done modulo 2^32.

### Payload and coder

```
u8      model version: 1 (a reader refuses any other value)
...     arithmetic coder stream
```

Let N be the decompressed size (block header). The N bytes are coded bit
by bit, from the most significant bit to the least significant one.
Coder: x1 = 0, x2 = 2^32 − 1. For each bit b, of probability p (on 12
bits, from 1 to 4095, that b is 1): xmid = x1 + ⌊(x2 − x1) · p / 4096⌋
(64-bit product); if b = 1 then x2 = xmid, otherwise x1 = xmid + 1; then
while the high bytes of x1 and x2 are equal: write that byte (x2's),
x1 = x1 << 8 (on 32 bits), x2 = (x2 << 8) | 255. At the end, x1 is
written on 4 bytes (big-endian). The decoder reads 4 bytes into x (bytes
beyond the end are 0), computes xmid the same way, b = 1 if x ≤ xmid,
updates x1 and x2 likewise, and for each byte shifted out does
x = (x << 8) | next byte.

### Functions and tables

- squash(x) (logistic function): if x > 2047, 4095; if x < −2047, 1;
  otherwise, with T = [1, 2, 3, 6, 10, 16, 27, 45, 73, 120, 194, 310, 488,
  747, 1101, 1546, 2047, 2549, 2994, 3348, 3607, 3785, 3901, 3975, 4024,
  4050, 4068, 4079, 4085, 4089, 4092, 4093, 4094], w = x & 127,
  i = (x >> 7) + 16: (T[i] · (128 − w) + T[i+1] · w + 64) >> 7.
- stretch(p), p from 0 to 4095: for x from −2047 to 2047 in order, the p
  from the end of the previous interval up to squash(x) included receive
  x; the remaining p receive 2047. For a 16-bit probability q:
  st(q) = stretch(q >> 4).
- div[n] = ⌊131072 / (2n + 3)⌋ for n from 0 to 1024.
- **Counter** (16-bit q, n): q starts at 32768 (unless stated), n at 0.
  Update by a bit b, with r = div[n]: if b = 1,
  q = q + ⌊(65535 − q) · r / 65536⌋, otherwise q = q − ⌊q · r / 65536⌋;
  then q is bounded to [32, 65503]; n increases by 1 while it is below
  127.
- **History states**: list L of pairs (n0, n1). A pair is allowed if
  max(n0, n1) ≤ 40 and min(n0, n1) ≤ f(max), with f(h) = h for h ≤ 4, 4
  for 5 to 7, 3 for 8 to 12, 2 for 13 to 20, 1 beyond. L starts with
  (0, 0), then for t = 1 to 80, for n0 = 0 to t: (n0, t − n0) if it is
  allowed (223 states). The state of a byte is its rank in L. Transition
  by a bit b: the count of b increases by 1; if the other count exceeds
  2, it becomes ⌊other / 2⌋ + 1; both are bounded to 40; while the pair is
  not allowed, the other count decreases by 1.

### Model state

- c0: current byte, preceded by a 1 (1 at the start of each byte); k:
  number of bits of this byte already read; c4: the last 4 bytes (on 32
  bits, the most recent in the low byte); d[1..]: bytes already seen
  (d[1] the most recent, 0 before the start); i: number of bytes seen.
- Sizes: B = bit length of N (0 for N = 0); history table of 2^T slots of
  16 bytes with T = min(max(B + 5, 16), 22); repetition table of 2^M
  positions (on 32 bits, zeroed) with M = min(max(B + 1, 12), 22).
- History table H: 16 · 2^T bytes (stored slots), then 256 (order 0),
  then 65,536 (order 1), all zero at the start.
- 15 learning tables (one per context) of 256 counters: the counter of
  state s = (n0, n1) starts at q = ⌊65535 · (2n1 + 1) / (2n0 + 2n1 + 2)⌋
  (for ranks beyond L: n0 = n1 = 0).
- Word: w = 0 and w' = 0 at the start. Lines: line start D = 0, previous
  line start D' = 0. Indirect: table I of 65,536 16-bit values, zeroed.
- Repetitions: length ℓ = 0, pointer π; 64 counters R.
- Three mixers of 33 weights per set (initial weight 16384): 256 sets,
  256 sets, 256 sets. Three refinements: 256, 65,536 and 65,536 contexts
  of 33 16-bit values, initial value of index j: squash((j − 16) · 128) ·
  16.

### After each byte c

1. I[c4 & 0xFFFF] = (I[c4 & 0xFFFF] << 8) | c (on 16 bits); append c;
   c4 = (c4 << 8) | c.
2. Word: let l be the ASCII lower case of c. If l is an ASCII letter or
   digit, or c ≥ 128, or c = "_": w = (w xor l) · 0x01000193; otherwise if
   w ≠ 0: w' = w, w = 0.
3. If c = 0x0A: D' = D, D = i (number of bytes seen, c included).
4. col = i − D; above a = the byte of rank D' + col (counted from 0) if
   D' + col < D, otherwise 0.
5. With m(v, k) = ((v + k) · 0x9E3779B1) xor (k · 0x85EBCA6B), and
   P = 0x2C1B3C6D, the 13 context hashes (in this order):
   m(c4 & 0xFFFF, 2), m(c4 & 0xFFFFFF, 3), m(c4, 4),
   m(c4 xor ((d5 | d6 << 8) · P), 6), m(w xor (d1 << 24), 7),
   m(w + w' · P, 8), m(a | min(col, 255) << 8 | d1 << 16, 9),
   m(d2 | d3 << 8, 10), m(c4 xor (d5 · P), 5),
   m(c4 xor ((d5 | d6 << 8 | d7 << 16 | d8 << 24) · P), 11),
   m(d4 | d8 << 8 | (i & 3) << 16, 12), m(I[c4 & 0xFFFF] | d1 << 16, 13),
   m(d1 | d3 << 8, 14).
6. Repetitions: if ℓ > 0: π = π + 1, ℓ = min(ℓ + 1, 65535). If i ≥ 7:
   h = (FNV hash of the last 7 bytes: h = 0, then for each one
   h = (h xor byte) · 0x01000193) >> (32 − M); if ℓ = 0 and the position
   p = table[h] is non-zero: count λ, the number of equal bytes going back
   (byte of rank p − 1 − j against byte of rank i − 1 − j, for j < 64 and
   j < p); if λ ≥ 7: ℓ = λ, π = p. Then table[h] = i. Finally if ℓ > 0
   and π ≥ i: ℓ = 0.

### Predicting a bit

1. At the start of each nibble (k = 0 or 4), for each hash e of step 5:
   h = (e xor (c0 · 0x9E3779B1)) · 0x2C1B3C6D; h = (h xor (h >> 15)) ·
   0x85EBCA6B; h = h xor (h >> 13) (logical shifts, on 32 bits). Slot:
   z = (h & (2^T − 1)) · 16, slot check e8 = h >> 24. Among z, z xor 16,
   z xor 32, the first one whose byte 0 equals e8 is kept; otherwise the
   one whose byte 1 is the smallest (the first one in case of a tie) is
   reset to zero (16 bytes), its byte 0 receives e8, and it is kept.
2. Rank in the nibble: u = c0 if k < 4, otherwise (c0 & (2^(k−4) − 1)) |
   2^(k−4). Contexts are numbered from 0 to 14: 0 to 12 in the order of
   the hashes of step 5, 13 = order 0, 14 = order 1. History of context
   j: H[slot_j + u] for the 13 stored contexts, H[16 · 2^T + c0] (order
   0), H[16 · 2^T + 256 + (d1 << 8 | c0)] (order 1).
3. Inputs x[0..33]: for each context j (0 to 14), of state s: if s = 0,
   x[2j] = x[2j+1] = 0; otherwise, q being counter s of its table:
   x[2j] = st(q), x[2j+1] = ((q >> 4) − 2048) >> 1.
4. Repetition: if ℓ > 0 and (byte of rank π | 256) >> (8 − k) = c0:
   expected bit e = ((byte | 256) >> (7 − k)) & 1; λ' = ℓ if ℓ < 16,
   otherwise 16 + (min(ℓ, 1023) >> 6); counter R[min(λ', 31) · 2 + e] =
   q'; x[30] = st(q'), x[31] = ±min(ℓ, 32) · 32 (+ if e = 1). If ℓ > 0
   without a match: ℓ = 0. Without an active repetition: x[30] = x[31] =
   0, e = 0. x[32] = 256.
5. Mixers: sets m1 = c0; m2 = ((g · 8 + k) · 2 + e) · 4 + (d1 >> 6), with
   g = 0 if ℓ = 0, 1 if ℓ < 16, 2 if ℓ < 32, 3 otherwise; m3 = d1. For
   each: s = clamp(Σ w[i] · x[i] >> 16, −2047, 2047) (32-bit products,
   64-bit sum), pm = squash(s). S = (s1 + s2 + s3) / 3; P0 = squash(S).
6. Refinement (context χ): v = S + 2048, l = v >> 7, ω = v & 127;
   A = (t[χ][l] · (128 − ω) + t[χ][l+1] · ω) >> 11; the index updated
   afterwards is l + 1 if ω ≥ 64, otherwise l. Contexts: a1: c0; a2:
   c0 | d1 << 8; a3: c0 | (((c4 & 0xFFFF) · 0x9E3779B1) >> 24) << 8.
7. p = clamp((P0 + a1 + 3 · a2 + 3 · a3 + 4) >> 3, 1, 4095).

### Learning after bit b

1. Each mixer: err = (b << 12) − pm; each weight w[i] =
   clamp(w[i] + ((x[i] · err) >> 12), −(2^20 − 1), 2^20 − 1).
2. Each context: its counter s (table j) learns b (then the state becomes
   the transition of s by b in H).
3. If a repetition was active: the counter R used learns b.
4. Each refinement: t = t + ((65535 · b − t) >> 6) at the kept index.
5. c0 = 2 · c0 + b, k = k + 1; at 8 bits: "after each byte" processing,
   c0 = 1, k = 0.
