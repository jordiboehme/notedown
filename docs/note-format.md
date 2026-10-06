# Supernote `.note` file format

This document describes the binary format of Supernote `.note` notebook files as far as it could be established from four sample files. It is written so that a parser can be implemented from this text and the samples alone. The open-source Python package supernotelib (0.7.3) was used as a reference for facts about the format; nothing in this document is derived from its code structure.

## 0. Conventions

### Evidence marks

Every statement carries one of these marks:

| Mark | Meaning |
|---|---|
| [verified] | Checked on the sample files with an independent decoder written from this text. |
| [reference] | Taken from the reference implementation only; not observed in the samples. |
| [others] | Reported by other community parsers (see Appendix B); not observed in the samples. |
| [inferred] | Reasoning from the samples that is consistent but not proven. |
| [unknown] | Present in the samples, meaning not established. A parser should expose it as raw data. |

### Sample files

| Short name | Path | Signature | Device code | Pages | Notes |
|---|---|---|---|---|---|
| t1 | `tests/fixtures/notes/test1.note` | `SN_FILE_VER_20260016` | `N6` | 1 | real-time recognition note, ruled template, 8 saves in the file |
| t2 | `tests/fixtures/notes/test2.note` | `SN_FILE_VER_20260016` | `N6` | 1 | two drawn diagrams, ruled template, one erased stroke |
| bike | `tests/fixtures/local/bike.note` (local only) | `SN_FILE_VER_20230015` | `N6` | 3 | compacted file, no stale data |
| content | `tests/fixtures/local/content.note` (local only) | `SN_FILE_VER_20230015` | `A6X` | 2 | 6 links to other notes, 15 saves, erased strokes, lasso records |

The handwritten content of the samples is private. This document describes structure only. Worked examples use t1 and t2 where possible; for bike and content only counts, addresses and hashes are given.

### Byte order and types

All integers are little-endian [verified]. `u8`, `u16`, `u32`, `i16`, `i32`, `u64` have their usual meaning; `f32` and `f64` are IEEE 754 little-endian floats [verified]. An "address" is a `u32` absolute byte offset from the start of the file [verified].

## 1. File envelope

| Offset | Size | Content |
|---|---|---|
| 0 | 4 | File type, ASCII. `note` in all samples [verified]. `mark` for PDF annotation files [others]. |
| 4 | 20 | Signature, ASCII `SN_FILE_VER_` followed by 8 decimal digits [verified]. |
| 24 | ... | First block (in all samples a header block) [verified]. |
| end-8 | 4 | ASCII `tail` [verified]. |
| end-4 | 4 | `u32` address of the current footer block [verified]. |

### Known signatures

| Signature | Status |
|---|---|
| `SN_FILE_VER_20230015` | [verified] bike, content |
| `SN_FILE_VER_20260016` | [verified] t1, t2 |
| `SN_FILE_VER_20200001`, `...20200005`, `...20200006`, `...20200007`, `...20200008`, `...20210009`, `...20210010`, `...20220011`, `...20220013`, `...20230014` | [reference] older firmware generations |
| `SN_FILE_ASA_20190529` | [reference] original A5. The signature starts at offset 0 (it replaces the file type field), there are no layers, pages use `DATA`/`PROTOCOL` keys and the footer repeats a plain `PAGE` key. |

The digits are a date-like version number (`YYYY` + 4-digit sequence) [inferred]. A version of `20230015` or higher selects the "X2" color code table for RATTA_RLE bitmaps (section 5.3) [reference; consistent with all samples].

### Differences between 20230015 and 20260016 in practice

No difference in container structure, block encoding, metadata syntax, RLE or stroke layout was found [verified]. Differences seen:

- The stroke record trailer is 4 bytes longer in 20260016 files (section 9.3, part P) [verified].
- Some metadata keys appear only in blocks written by newer firmware: page keys `PAGETEXTBOX`, `DISABLE`; link keys `ANTI`, `BOLD`, `ITALIC`. They depend on the firmware that wrote the block, not on the file signature: content (20230015) has them in blocks written in 2026 and lacks them in older blocks of the same file [verified].
- The first header written at creation can be a short variant without `MODULE_LABEL`, `APP_VERSION`, `PDFSTYLE`, `PDFSTYLEMD5`, `STYLEUSAGETYPE`, `HIGHLIGHTINFO` (t1 at 24, t2 at 24, 302 bytes); later saves write the full header (t1 at 3194, 402 bytes) [verified].

### Unknown signatures

A reader should accept any `SN_FILE_VER_` + 8 digits and parse it with these rules, flagging the version as unrecognized rather than failing [inferred]. The block structure has been stable across all versions the reference knows [reference].

## 2. Block chain and append-only saves

### Blocks

Everything after the 24-byte prefix is a sequence of blocks, each:

| Size | Content |
|---|---|
| 4 | `u32` length N |
| N | payload |

A block is referenced by the address of its length field [verified]. There are two payload kinds: metadata blocks (UTF-8 text of `<KEY:VALUE>` pairs, section 3) and binary blocks (bitmaps, stroke data, ZIP archives) [verified].

### Save markers

Each save appends new blocks and ends with a new footer block followed by the 8 bytes `tail` + `u32` footer address [verified]. Walking the file from offset 24, reading block after block and skipping each 8-byte `tail`+address marker, consumes all four sample files exactly to their last byte [verified]:

| File | Blocks | Save markers | Stale footers |
|---|---|---|---|
| t1 | 25 | 8 | 7 |
| t2 | 14 | 3 | 2 |
| bike | 19 | 1 | 0 |
| content | 70 | 15 | 14 |

A sequential walk is not needed for parsing; it is useful for validation and for exposing history [inferred].

### Finding the current state

1. Read the `u32` at file length minus 4: the current footer address [verified].
2. The 4 bytes before it should be `tail` [verified].
3. Parse the footer block. All other current blocks are reached by addresses from the footer, recursively [verified].

### Stale blocks

Blocks that are not reachable from the current footer are stale: earlier headers, page blocks, layer blocks, bitmaps, TOTALPATH blocks and footers from previous saves [verified]. A reader must ignore them for the current state. Reachable blocks may be old: content's current page 1 still uses a BGLAYER block at 31288 written in its first save [verified]. So "stale" means "unreachable", never "earlier than the last marker".

Stale data is real history. Examples [verified]:

- t1: header at 24 (`FILE_RECOGN_TYPE:0`), header at 2704 (`FILE_RECOGN_TYPE:0`, full form), header at 3194 (`FILE_RECOGN_TYPE:1`, current). The user switched the note to real-time recognition after creation.
- t1 page history: page 1384 (empty), 3684 (`RECOGNSTATUS:2`), 49103 (new MAINLAYER bitmap, `TOTALPATH:0`), 155303 (`TOTALPATH:50458`), 183919 (`RECOGNFILE:156663`), 185284 (`RECOGNFILESTATUS:1`, current).
- content: 15 saves; the MAINLAYER bitmap of page 1 was replaced 6 times.

A compacted file (bike) has exactly one save marker and its blocks are in the order header, template bitmaps, then per page: layer bitmap, layer metadata blocks, TOTALPATH, page block; then the footer [verified].

### Ordering of bitmap and vector data inside a save cycle

In t1, t2 and content the device writes a new layer bitmap and a page block whose `TOTALPATH` still points to the old stroke data (or is `0`), and only in a later save writes the new TOTALPATH block and another page block [verified: t1 page 49103, t2 page 81216, content pages 319303, 535616, 748179, 966538, 1222374, 1472685]. Consequence: in a file captured between these two saves, the vector data lags the bitmap [inferred]. See section 9.7.

## 3. Metadata blocks

### Syntax

The payload is UTF-8 text, a concatenation of items `<KEY:VALUE>` with no separators [verified].

- KEY is non-empty and contains no `:`, `<` or `>` [verified on all keys].
- VALUE runs from the first `:` after the key to the next `>` [verified]. Values in the samples never contain `<` or `>` [verified]. There is no escaping mechanism [verified: none observed]. The JSON in `LAYERINFO` avoids `:` by replacing it with `#` (section 4.4) [verified]. Base64 values contain `=` and `/` but no `<` or `>` [verified].
- A value may contain `:` (none observed except inside the `#`-escaped JSON) [inferred: take everything up to `>`].
- A key may repeat inside one block (footer `TITLE_`, `KEYWORD_`, `LINKO_` keys when two items share a position, original A5 `PAGE`) [reference]. No repeated keys in the samples [verified]. A parser should keep all values in order.
- Item order is meaningful for repeated keys only [inferred]; footer `PAGEn` keys appear in page order in the samples [verified], but a parser should sort by the number in the key.

### Value types

| Type | Examples | Notes |
|---|---|---|
| decimal integer | `24`, `0`, `-1` | addresses, counts, flags [verified] |
| address | `185284` | `0` means "absent" [verified] |
| literal `none` | `none` | absent string value [verified] |
| decimal float | `0.000000` | `FONTSIZE` [verified] |
| enumeration word | `NOTE`, `RATTA_RLE`, `MAINLAYER` | [verified] |
| id | `F20261005225333089589QOPqpMYT9WQV` | section 3.1 [verified] |
| timestamp | `20240425085032525691` | `YYYYMMDDhhmmss` + 6-digit microseconds [verified by shape and by match with IDs] |
| rect | `812,246,537,84` | `left,top,width,height` in page pixels [verified, section 7] |
| comma list | `MAINLAYER,BGLAYER` | [verified] |
| base64 | `L3N0b3Jh...` | UTF-8 path or URL [verified] |
| JSON with `#` for `:` | `LAYERINFO` | [verified] |

### 3.1 Identifiers and timestamps

- File id `FILE_ID`: `F` + 20 digits + 12 alphanumeric characters, e.g. `F20240708174639550429bHksqaP2wKQ8` (bike) [verified]. The digits are a creation timestamp `YYYYMMDDhhmmss` + 6-digit microseconds [verified: t1's id `F20261005225333...` versus its recognition data creation date `2026-10-05 20:53:33` UTC]. The timestamp is device local time (UTC+2 for t1) [inferred]. The suffix is random [inferred].
- Page id `PAGEID`: `P` + same shape, the page creation time [verified by shape]. For page 1 it is a few milliseconds after the file id [verified: t1, t2].
- Link timestamp `LINKTIMESTAMP`: 20 digits, same timestamp shape, no prefix [verified].
- No other per-page or per-stroke timestamps exist in the metadata [verified]. Per-stroke timestamps exist only inside the recognition archive (section 8.3).

## 4. Logical structure

```
footer
 ├─ FILE_FEATURE  -> header block
 ├─ PAGEn         -> page block (n = 1..)
 │    ├─ MAINLAYER, LAYER1, LAYER2, LAYER3, BGLAYER -> layer block -> LAYERBITMAP -> bitmap
 │    ├─ TOTALPATH  -> stroke data (section 9)
 │    ├─ RECOGNFILE -> ZIP archive (section 8)
 │    └─ RECOGNTEXT -> recognition text (section 8)
 ├─ STYLE_<name>  -> template bitmap
 ├─ TITLE_<pos>   -> title block -> TITLEBITMAP
 ├─ KEYWORD_<pos> -> keyword block -> KEYWORDSITE
 ├─ LINKO_<pos>   -> link block -> LINKBITMAP
 └─ COVER_n       -> cover image
```

### 4.1 Footer keys

| Key | Value | Meaning | Mark |
|---|---|---|---|
| `PAGE1`, `PAGE2`, ... | address | page blocks, 1-based page number in the key | [verified] |
| `FILE_FEATURE` | address | header block | [verified] |
| `DIRTY` | integer | 0 in bike (compacted), 1 in t1/t2, 6 in content; rises with unsaved edits | [verified values], meaning [others: count of modifications since last compaction] |
| `COVER_0` | `0` | no cover | [verified] |
| `COVER_1`, `COVER_2` | address | cover image; `COVER_2` preferred over `COVER_1` | [reference] |
| `STYLE_<stylename>` | address | template bitmap per style name, e.g. `STYLE_style_8mm_ruled_line:330`, `STYLE_style_white:905` | [verified] |
| `STYLE_user_<name><md5>` | address | custom template (PNG), md5 appended to the name | [reference] |
| `TITLE_<page4><top4><left4>` | address | title block | [reference]; shape [others] |
| `KEYWORD_<page4><top4>` | address | keyword block | [reference]; shape [others] |
| `LINKO_<page4><top4><left4><height4><width4>` | address | link block | [verified] |

The position digits in `LINKO_` keys equal the link's `LINKRECT` (section 7.3) [verified on all 6 links]: `LINKO_00010246081200840537` = page 0001, top 0246, left 0812, height 0084, width 0537 for `LINKRECT:812,246,537,84`. The page number for titles, keywords and links must be taken from the key [verified for links; reference for titles; others report `KEYWORDPAGE` can be wrong].

t1 and t2 carry no `COVER_` key at all [verified]; bike and content carry `COVER_0:0` [verified].

### 4.2 Header keys

| Key | Sample values | Meaning | Mark |
|---|---|---|---|
| `MODULE_LABEL` | `none` (t1), `SNFILE_FEATURE` (bike, content) | unknown | [unknown] |
| `FILE_TYPE` | `NOTE` | file kind | [verified] |
| `APPLY_EQUIPMENT` | `N6`, `A6X` | device model code, selects page size (4.6) | [verified] |
| `FINALOPERATION_PAGE` | `1`, `3`, `2` | last page edited (1-based) | [inferred: equals a valid page] |
| `FINALOPERATION_LAYER` | `1` | last layer edited | [inferred] |
| `DEVICE_DPI` | `0` | 0 on these devices | [verified value], meaning [others] |
| `SOFT_DPI` | `0` | as above | [verified value] |
| `FILE_PARSE_TYPE` | `0` | unknown | [unknown] |
| `RATTA_ETMD` | `0` | unknown | [unknown] |
| `APP_VERSION` | `0` | unknown | [unknown] |
| `FILE_ID` | see 3.1 | file identity, target of links | [verified] |
| `FILE_RECOGN_TYPE` | `0`, `1` | `1` = real-time recognition note | [verified with t1 history], meaning [reference] |
| `FILE_RECOGN_LANGUAGE` | `none`, `en_US` | recognition language | [verified] |
| `PDFSTYLE` | `none` | PDF used as template | [unknown] |
| `PDFSTYLEMD5` | `0` | hash of that PDF | [unknown] |
| `STYLEUSAGETYPE` | `0` | unknown | [unknown] |
| `HIGHLIGHTINFO` | `0` | address of highlight data, base64 JSON | value [verified]; meaning [others] |
| `HORIZONTAL_CHECK` | `0` | unknown | [unknown] |
| `IS_OLD_APPLY_EQUIPMENT` | `1` (t1, t2), `0` (bike, content) | unknown | [unknown] |
| `ANTIALIASING_CONVERT` | `2` | anti-aliased bitmaps (section 5.3) | value [verified], meaning [inferred] |

### 4.3 Page keys

| Key | Sample values | Meaning | Mark |
|---|---|---|---|
| `PAGESTYLE` | `style_8mm_ruled_line` | template name; `style_white` = blank; `user_...` = custom PNG | [verified] for the first two, [reference] for `user_` |
| `PAGESTYLEMD5` | `0` | hash of custom template, `0` = none | [verified value] |
| `LAYERINFO` | JSON, see 4.4 | layer names, visibility, flags | [verified] |
| `LAYERSEQ` | `MAINLAYER,BGLAYER` | layer stacking order, first = top | [verified] |
| `MAINLAYER`, `LAYER1`, `LAYER2`, `LAYER3`, `BGLAYER` | address or `0` | layer blocks | [verified] |
| `TOTALPATH` | address or `0` | stroke data, section 9 | [verified] |
| `THUMBNAILTYPE` | `0` | unknown; no thumbnail blocks exist in the samples | [unknown] |
| `RECOGNSTATUS` | `0`, `2` | 0 none, 1 done, 2 running | values [verified], meaning [reference] |
| `RECOGNTEXT` | `0` | address of recognized text | [verified value 0]; format [reference] |
| `RECOGNFILE` | `0`, `156663` | address of recognition ZIP | [verified] |
| `PAGEID` | see 3.1 | page identity, link target | [verified] |
| `RECOGNTYPE` | `0` | unknown | [unknown] |
| `RECOGNFILESTATUS` | `0`, `1` | 1 after the RECOGNFILE was written (t1 history) | [verified sequence], meaning [inferred] |
| `RECOGNLANGUAGE` | `none` | per-page language | [unknown] |
| `EXTERNALLINKINFO` | `0` | address, unknown content | [unknown] |
| `IDTABLE` | `0` | address, unknown content | [unknown] |
| `ORIENTATION` | `1000` | `1000` portrait; `1090` and `1270` landscape | `1000` [verified], others [reference] |
| `PAGETEXTBOX` | `0` | text boxes present | [others] |
| `DISABLE` | `none` | regions of bitmap-only content (text boxes, digests) | [others] |
| `FIVESTAR` | absent | present only on starred pages; value holds the star stroke coordinates and a trailing flag, exact layout not documented | [others] |

Keys of a page block written by newer firmware: `PAGETEXTBOX`, `DISABLE` [verified present in t1, t2, content page 1; absent in bike and content page 2].

All address-valued page keys with value `0` mean "absent" [verified].

### 4.4 LAYERINFO

The value is JSON with every `:` replaced by `#` [verified]. Replace `#` by `:` and parse as JSON (the reference also accepts base64-encoded JSON as a fallback [reference]). In the samples it is an array of 5 objects [verified], in t1:

| layerId | name | isBackgroundLayer | isCurrentLayer | isVisible | isDeleted |
|---|---|---|---|---|---|
| 3 | `Layer 3` | false | false | true | true |
| 2 | `Layer 2` | false | false | true | true |
| 1 | `Layer 1` | false | false | true | true |
| 0 | `Main Layer` | false | true | true | false |
| -1 | `Background Layer` | true | false | true | false |

Further fields per entry: `isAllowAdd`, `isAllowUp`, `isAllowDown` (booleans) [verified]. Mapping: `layerId` 0 = MAINLAYER, 1..3 = LAYER1..3, `isBackgroundLayer:true` = BGLAYER [reference; consistent]. A layer is drawn when `isVisible` is true [reference]. Layers 1-3 are marked deleted and have address `0` in the page block [verified]. Names are user-editable [others]. If MAINLAYER is missing from LAYERINFO it is visible [reference]. When `LAYERINFO` is `none`, visibility comes from `LAYERTYPE` (layers of type `MARK` visible) [reference; for `.mark` files].

### 4.5 Layer keys

| Key | Sample values | Meaning | Mark |
|---|---|---|---|
| `LAYERTYPE` | `NOTE` | `NOTE` or `MARK` | [verified] / [reference] |
| `LAYERPROTOCOL` | `RATTA_RLE` | bitmap encoding | [verified] |
| `LAYERNAME` | `MAINLAYER`, `BGLAYER` | layer slot name | [verified] |
| `LAYERPATH` | `0` | address of per-layer path data | [unknown] |
| `LAYERBITMAP` | address | bitmap block | [verified] |
| `LAYERVECTORGRAPH` | `0` | address of vector graph data | [unknown] |
| `LAYERRECOGN` | `0` | address of per-layer recognition | [unknown] |

Some firmware writes the background layer with `LAYERNAME:MAINLAYER`; a second MAINLAYER-named layer is then the background layer [reference]. A parser should identify layers by the page key that points to them (`BGLAYER:<addr>`), not by `LAYERNAME` [inferred].

### 4.6 Page size by device

| `APPLY_EQUIPMENT` | Device | Portrait width x height | Mark |
|---|---|---|---|
| `N6` | Supernote Nomad (A6 X2) | 1404 x 1872 | [verified] |
| `A6X` | Supernote A6X | 1404 x 1872 | [verified] |
| `N5` | Supernote Manta (A5 X2) | 1920 x 2560 | [reference] |
| any other | A5X, A5 and others | 1404 x 1872 | [reference] |

Every bitmap in the samples decodes to exactly 1404 x 1872 = 2,628,288 pixels [verified]. The RLE stream itself does not state the size; the decoder needs it from the device code [verified]. For landscape pages (`ORIENTATION` 1090 or 1270) the bitmap is stored with width and height swapped, i.e. 1872 wide and 1404 high [reference]. The 300 dpi of the 1404 x 1872 devices follows from the stroke data (section 9.4) [verified: 0.0845 mm per pixel].

## 5. Bitmaps

### 5.1 Protocols

| Protocol name | Where | Encoding | Mark |
|---|---|---|---|
| `RATTA_RLE` | `LAYERPROTOCOL`, `LINKPROTOCAL`, `TITLEPROTOCOL` | run-length, 5.2 | [verified] |
| `SN_ASA_COMPRESS` | original A5 `PROTOCOL` | zlib stream of `u16` pixels, 1404 columns x 1888 rows stored rotated; rotate 90 degrees clockwise and drop the last 16 rows; codes `0x0000` black, `0x2104` dark gray, `0xE1E2` gray, `0xFFFF` background | [reference] |
| PNG | BGLAYER when `PAGESTYLE` starts with `user_` | a complete PNG file as block payload | [reference] |

Only RATTA_RLE occurs in the samples (14 layer bitmaps, 4 template bitmaps, 6 link bitmaps) [verified]. A binary block that starts with `89 50 4E 47` is a PNG [reference]; RLE blocks have no magic.

### 5.2 RATTA_RLE encoding

The payload is a sequence of 2-byte pairs `(code, len)` [verified]. Pixels are produced in row-major order, top row first, left to right [verified by pixel-exact match]. A layer with page size W x H produces exactly W*H pixels [verified on all 18 layer and template bitmaps].

Meaning of a pair, by the value of `len`:

| `len` | Meaning |
|---|---|
| `0x00`-`0x7F` | a run of `len + 1` pixels (1 to 128) of `code` [verified] |
| `0xFF` | a run of 16384 (`0x4000`) pixels of `code` [verified] |
| `0x80`-`0xFE` | a "high part" `h = len & 0x7F` (0 to 126). See below. [verified] |

High part. A pair with a high part does not stand alone; its meaning depends on the next pair:

- If the next pair has the same `code` and a `len` of `0x00`-`0x7F` (call it `l`), the two pairs together encode one run of `(h + 1) * 128 + l + 1` pixels (129 to 16384) [verified, 8,468 such combinations in the current layers of the samples].
- If the next pair has a different `code`, the high-part pair alone is a run of `(h + 1) * 128` pixels, and the next pair is read normally [verified, 63 occurrences].
- A high-part pair followed by a same-code pair whose `len` is `0x80` or more never occurs in the samples [verified]. Treat it as malformed [inferred]; the reference is lenient there [reference].
- A high-part pair as the last pair of the stream never occurs in the samples; every sample stream ends with a complete run [verified]. Treat it as malformed or as a run that is cut to the remaining pixel count [inferred]; the reference accepts it and shortens the run to fit [reference].

Special blank length. The reference uses 1024 instead of 16384 for `0xFF` in one case: a BGLAYER of a `style_white` page whose bitmap block is exactly 5134 (`0x140E`) bytes long [reference]. The samples' `style_white` bitmap is 324 bytes and decodes correctly with 16384 [verified]. Treat the 1024 rule as a compatibility quirk of older files.

Streams of odd length do not occur [verified]. A run may cross row boundaries [verified].

#### Worked examples

t1 BGLAYER bitmap at 330 (462 bytes), first 30 bytes [verified]:

```
62 ff  x 11          11 runs of 16384 transparent       = 180224
62 86  62 6d         (6+1)*128 + 0x6d+1 = 896 + 110      = 1006 transparent
61 88  61 17         (8+1)*128 + 0x17+1 = 1152 + 24      = 1176 black
```

After 181230 transparent pixels (row 129, column 114) follows a horizontal ruling line of 1176 black pixels (columns 114 to 1289). The whole template has 18 such lines [verified: 21168 = 18 x 1176 black pixels].

t1 MAINLAYER bitmap at 5037, bytes 11616-11619: `62 86 df 00` = 896 transparent pixels (high part followed by a different code), then 1 pixel of code `0xDF` [verified].

`style_white` bitmap (t1 block 796, bike 905, content 2488, byte-identical, 324 bytes): 160 pairs `62 ff`, then `62 b4 62 3f` = (0x34+1)*128 + 0x3f+1 = 6848; total 160*16384 + 6848 = 2,628,288 transparent pixels [verified].

### 5.3 Color codes

Two code tables exist. The "X2" table applies when the signature version is 20230015 or higher [reference; all samples are X2]. Values below are the gray levels produced by the reference renderer, which the golden images use [verified pixel-exactly for every code that occurs].

| Code | Name | Gray (X2 table) | Gray (X table, older) | Seen in samples |
|---|---|---|---|---|
| `0x61` | black | 0 | 0 | yes |
| `0x62` | transparent (background) | transparent | transparent | yes |
| `0x63` | dark gray (X), compat dark gray (X2) | 0x30 | 0x9D | yes, see note |
| `0x64` | gray (X), compat gray (X2) | 0x50 | 0xC9 | yes, see note |
| `0x65` | white | 0xFE | 0xFE | no |
| `0x66` | marker black | 0 | 0 | yes (2 pixels in t1) |
| `0x67` | marker dark gray (X only) | (literal) | 0x9D | no |
| `0x68` | marker gray (X only) | (literal) | 0xC9 | no |
| `0x9D` | dark gray (X2) | 0x9D | n/a | no |
| `0x9E` | marker dark gray (X2) | 0x9D | n/a | no |
| `0xC9` | gray (X2) | 0xC9 | n/a | no |
| `0xCA` | marker gray (X2) | 0xC9 | n/a | no |
| any other | anti-aliasing level (X2) | the code value itself | n/a | yes |

Anti-aliasing [verified]: all X2 sample layers contain edge pixels with codes outside the named set. Pen strokes use mostly the 15 levels `0x0F, 0x1F, ..., 0xEF` (step 16); t1 also has scattered other values (`0x03`, `0x05`, `0x11`, ... `0xD1`). These are gray levels: the golden renderings show them with gray = code [verified].

Note on `0x63`/`0x64` [inferred, strong]: in content page 1 these two codes occur only inside the link icons that the device draws into MAINLAYER (24 and 12 pixels), surrounded by levels `0x10, 0x20, ..., 0xE0`, `0x60`, `0x70`. There they are plain anti-aliasing levels between 0x60 and 0x70, not "compat" colors. The reference maps them to 0x30 and 0x50 and the goldens follow it. To reproduce the goldens use 0x30/0x50; for a faithful image use the code value [inferred].

The code `0x62` as a literal gray level would be indistinguishable from transparent; levels in the samples never fall on `0x61`-`0x66` except the two link icon cases above [verified].

### 5.4 Template layer (BGLAYER)

The BGLAYER bitmap is the page template [verified]. Its block is shared: every page with the same `PAGESTYLE` points to the same bitmap block, and the footer lists it as `STYLE_<name>` [verified: bike pages 1-3 all use 439]. The ruled template `style_8mm_ruled_line` is 462 bytes and byte-identical in all four files [verified]. Templates in the samples contain only black (`0x61`) and transparent pixels [verified]. Footers may list templates no page uses (bike and content list `STYLE_style_white`) [verified].

## 6. Rendering a page

### 6.1 Compositing (matches the golden renderings)

1. Decode each layer with an address other than 0 to W*H codes [verified].
2. Determine visible layers from `LAYERINFO` (4.4) [reference; all layers visible in the samples].
3. Start with a canvas of gray 255 (white) [verified].
4. Go through `LAYERSEQ` from last to first (bottom to top). For each visible layer, every pixel whose code is not `0x62` replaces the canvas pixel with the code's gray value from 5.3. Transparent pixels leave the canvas unchanged [verified].
5. The result is an 8-bit gray image. The goldens are RGBA PNGs with R = G = B = gray and A = 255 [verified].

Without template ("nobg" goldens): skip BGLAYER, and output A = 0 with R = G = B = 255 where no layer covered the pixel [verified].

With these rules all 14 golden PNGs (7 pages, with and without template) are reproduced with 0 differing pixels [verified]. The order matters where ink overlaps template lines: in t1, 169 MAINLAYER pixels lie on template lines and the MAINLAYER value wins [verified].

Replacement compositing means anti-aliased edge pixels show their own level even over a black template line. A reader that wants alpha blending must do it itself (6.2).

### 6.2 How the device's own PNG export differs

The device exports in `tests/fixtures/device/` differ from the goldens [verified]:

- Black (`0x61` and `0x66`) is exported as gray 1, not 0.
- Where an anti-aliasing pixel of MAINLAYER lies over a black template pixel, the device writes `round(level * 1 / 255)` (0 for levels below 128, 1 above): the levels act like coverage values multiplied with what is below (83 pixels in t1 and 209 in t2, all consistent) [verified]. Over white the level is kept.
- Transparent is exported as 255.

So the device composites `result = level * below / 255` for anti-aliasing codes [inferred from the over-black and over-white cases; intermediate backgrounds not observed].

### 6.3 Other images

- Link bitmaps are RATTA_RLE (`LINKPROTOCAL`) images of exactly `LINKRECT` width x height pixels, row-major [verified: the run totals of all six link bitmaps in content equal width x height of their rects, e.g. 537 x 84 = 45108]. The four links created in 2024 store an all-transparent bitmap (8, 8, 6 and 8 bytes) [verified]. The two newer ones (3918 and 5660 bytes) hold ink: for the first, every non-transparent pixel equals the MAINLAYER pixel at the same page position, and the page has extra pixels in the rect (link icon, other strokes) [verified]; the second differs from the current page in 1625 pixels, probably because the page was edited after the link was made [inferred]. So the link bitmap is a snapshot of the linked handwriting at creation time [inferred].
- Title bitmaps (`TITLEBITMAP`) are RLE images of the title region [reference]; their size is `TITLERECT` width x height by analogy with links [inferred].
- The `KEYWORDSITE` block holds the keyword text as UTF-8 [others], not a bitmap.
- Cover image (`COVER_1`/`COVER_2`): binary block. All surveyed parsers expose it only as raw bytes; its encoding is not documented anywhere [unknown].
- No thumbnails are stored in the samples (`THUMBNAILTYPE:0`) [verified].

## 7. Titles, keywords, links

None of the samples has titles or keywords [verified]. content has six links [verified].

### 7.1 Title block keys [reference/others]

`TITLESEQNO`, `TITLELEVEL` (heading level), `TITLERECT` and `TITLERECTORI` (`left,top,width,height` in page pixels), `TITLEBITMAP` (address of an RLE bitmap of the title region), `TITLEPROTOCOL` (`RATTA_RLE`), `TITLESTYLE` (7 digits `1BBBFFF`: background gray BBB and text gray FFF, e.g. `1000254`, `1157254`, `1201000`, `1000000` = hatched) [others]. Footer key `TITLE_<page4><top4><left4>`; the reference sorts titles by top then left from `TITLERECTORI` [reference].

### 7.2 Keyword block keys [reference/others]

`KEYWORDPAGE` (1-based; others report it can be 0, so prefer the footer key), `KEYWORDSEQNO`, `KEYWORDRECT`, `KEYWORDRECTORI`, `KEYWORDSITE` (address of a block holding the keyword text as UTF-8), `KEYWORDLEN`, `KEYWORD` (text) [reference/others]. The reference reads the keyword text from the `KEYWORDSITE` block rather than the inline `KEYWORD` value [reference]. Footer key `KEYWORD_<page4><top4>` [reference].

### 7.3 Link block keys

| Key | Sample values | Meaning | Mark |
|---|---|---|---|
| `LINKTYPE` | `1` (all 6) | link kind. `1` = link to another `.note` file | `1` [verified]; `0` = page link inside notes, `4` = web link [reference]; `2` = link to a non-note document [others] |
| `LINKINOUT` | `0` | `0` outgoing, `1` incoming | `0` [verified as outgoing]; `1` [reference] |
| `LINKBITMAP` | address | RLE image of the link rect, `LINKRECT` width x height (6.3) | [verified] |
| `LINKSTYLE` | `20000000` | unknown style code; also stored in the stroke record (9.6) | [unknown] |
| `LINKTIMESTAMP` | `20240425085032525691` | creation time, 3.1 | [verified] |
| `LINKRECT` | `812,246,537,84` | `left,top,width,height` in page pixels | [verified: for the four 2024 links the link-box stroke record's polygon is exactly this rectangle; for the two newer links the polygon is 1 px smaller in width and height (erratum, 2026-10-06)] |
| `LINKRECTORI` | same as `LINKRECT` | original rect before a move | [inferred] |
| `LINKPROTOCAL` (sic) | `RATTA_RLE` | protocol of `LINKBITMAP` | [verified] |
| `LINKFILE` | base64 | UTF-8 absolute device path of the target, e.g. `/storage/emulated/0/Note/Bike.note`; a URL for web links | path [verified]; URL [reference] |
| `LINKFILEID` | `F2024...` | `FILE_ID` of the target file; `none` when not applicable | [verified] |
| `PAGEID` | `none` | `PAGEID` of the target page; `none` for whole-file links | [verified value]; meaning [reference] |
| `OBJPAGE` | `0` | target page number | [others]; value [verified] |
| `FONTSIZE` | `0.000000` | text link font size | [unknown] |
| `FONTPATH` | `none` | text link font | [unknown] |
| `FULLTEXT` | `none` | text of a text link | [unknown] |
| `SHOWTEXT` | `none` | displayed text | [unknown] |
| `ANTI`, `BOLD`, `ITALIC` | `1`, `1`, `0` | text style flags, only in links created by newer firmware | [unknown] |

Link targets [verified]: content's third link has `LINKFILE` decoding to `/storage/emulated/0/Note/Bike.note` and `LINKFILEID:F20240708174639550429bHksqaP2wKQ8`, which is exactly bike's header `FILE_ID`. Resolve a link by `LINKFILEID` first (stable across renames and moves) and use the decoded path as a fallback [inferred]. A link is internal when `LINKFILEID` equals the file's own `FILE_ID` [reference]. All six `LINKFILE` values are device paths under `/storage/emulated/0/Note/` ending in `.note` [verified].

The device draws a link icon at the right end of the link rect into MAINLAYER [verified: content page 1, the icons lie at the right ends of the six rects]. A link also leaves records in the page's stroke data (9.6).

## 8. Real-time recognition

### 8.1 Flags

- `FILE_RECOGN_TYPE:1` in the header marks a real-time recognition note [verified: t1].
- `FILE_RECOGN_LANGUAGE` holds the locale, e.g. `en_US` [verified].
- Page `RECOGNSTATUS`: `0` none, `1` done, `2` running [reference]. t1 shows `2` in its final state [verified].
- Page `RECOGNFILESTATUS`: went `0` -> `1` after `RECOGNFILE` was written [verified].

### 8.2 RECOGNTEXT [reference]

When present, the block payload is base64 text of a UTF-8 JSON document with an `elements` array; elements of `"type": "Text"` carry `label` (recognized text, UTF-8) and `words`, each with `label` and an optional `bounding-box` with `x`, `y`, `width`, `height` [reference, others]. Words with label `\n` mark line breaks [others]. None of the samples has a RECOGNTEXT block (t1 has `RECOGNTEXT:0` with `RECOGNSTATUS:2`) [verified]. Word box units are not page pixels [others: a scale of about 11.9 per pixel at 1920 width is used by one parser].

### 8.3 RECOGNFILE [verified on t1]

The block payload is a ZIP archive (starts with `PK\x03\x04`) [verified]. t1's archive (27252 bytes) holds:

| Entry | Size | Content |
|---|---|---|
| `rel.json` | 135 | JSON, pages and objects index |
| `index.bdom` | 233 | binary document skeleton (magic `BDOM`) |
| `meta.json` | 225 | JSON: format version, `OS`, `languageLocaleIdentifier`, `Application:"iink"`, `Application_Version`, `lastModificationDate` (UTC) |
| `pages/<id>/ink.bink` | 72435 | captured ink (magic `BINK`) |
| `pages/<id>/style.css` | 20550 | CSS |
| `pages/<id>/page.bdom` | 23599 | recognition document model (magic `BDOM`) |
| `pages/<id>/meta.json` | 345 | JSON: `creationDate`, `lastModificationDate` (UTC), language |

This is the working state of the recognition engine for the page [inferred from `Application:"iink"`].

`ink.bink` [verified on t1, layout from others]: magic `BINK`, a header declaring 4 channels `X` (mm), `Y` (mm), `F`, `T` (ms), then at byte 160 two `u32` 1000, 1000, a `u32` 3, a `u8` 0, then `u32` stroke count, then per stroke: `u32` 0, `u64` timestamp in microseconds since the Unix epoch (UTC), `u32` a duration-like value (3145 for every t1 stroke), `u32` point count n, n x (`f32` x mm, `f32` y mm), n x `f32` force (all 0), n x `u32` sample index (0..n-1). After the strokes follows an element table (recognition tree) [others].

Verified on t1: 72 strokes, the same count and order as TOTALPATH. Point counts per stroke are the same for 70 of 72 strokes; strokes 39 and 63 have 3 points in `ink.bink` and 2 in TOTALPATH (erratum found by the parser implementation, 2026-10-06), so a reader must not assume equal counts; timestamps strictly increasing over 45.4 seconds; coordinates are the TOTALPATH points in millimeters (0.0843 to 0.0848 mm per page pixel, i.e. 300 dpi) [verified]. This is the only place in the format where per-stroke times were found.

`page.bdom` and `style.css`: [others] describe a string table and recognition lattice; not decoded here [unknown].

## 9. Stroke data (TOTALPATH)

TOTALPATH holds the vector strokes of a page. The reference does not decode it. Everything in this section was decoded from the samples; field names follow the evidence, and where other parsers agree that is noted.

### 9.1 Container [verified]

| Size | Content |
|---|---|
| 4 | `u32` record count R |
| R times: 4 | `u32` record length L |
| L | record |

The sum consumes the block exactly on all 7 pages [verified]. Counts: t1 72, t2 84, bike 190/129/52, content 76/48.

Records are in creation order [verified: the `id` field (9.3 part I) strictly increases in record order on all pages (by exactly 1 in t1 and t2; with gaps in bike page 1, ids 1 to 216 for 190 records, and content page 1), and the RECOGNFILE timestamps of t1 strictly increase in record order]. Gaps are ids of records that were removed [inferred]. Draw order is record order [inferred].

### 9.2 Coordinate systems [verified]

Points are stored in digitizer units, not pixels. Each record stores the digitizer range: `maxA` at offset 128 and `maxB` at offset 132 (15819 and 11864 on N6 and A6X) [verified].

For a point stored as (`a`, `b`) (two `u32`, `a` first):

```
pixel_x = (maxB - b) * W / maxB
pixel_y = a * H / maxA
```

with W x H the portrait page size (1404 x 1872) [verified]. So the first stored value is the vertical axis, the second is the horizontal axis mirrored. This is the bitmap's 90-degree rotated sensor frame. all 51,722 points of live ink records (tool 10, status 0) in all samples land on a non-transparent MAINLAYER pixel after rounding [verified]. The scale is the same on both axes: 15819/1872 = 11864/1404 = 8.45 units per pixel [verified], i.e. 10 micrometers per unit at 300 dpi [inferred; consistent with ink.bink mm values]. Rounding variants (floor versus round, `maxB - 1 - b`) make no measurable difference [verified]. Landscape pages and other devices were not available: use the per-record ranges, never constants [inferred]. Older A5X firmware is reported with different ranges (15725 x 20967) [others].

Outline polygons (9.3 part J) are already in page pixels, `f32` x then y, not mirrored [verified: link-box polygon equals `LINKRECT`].

### 9.3 Record layout [verified: every one of 651 records parses to its exact length]

Offsets are relative to the start of the record. "n" means a count read just before an array.

**A. Fixed head (212 bytes)**

| Offset | Type | Field | Sample values | Mark |
|---|---|---|---|---|
| 0 | u32 | tool | 10 ink stroke, 3 eraser, 4 lasso, 0 link box | values [verified]; tool names: 10 needle point pen [others]; 3 eraser and 4 lasso [verified by behavior] |
| 4 | u32 | color | 0 black, 255 eraser | [verified]; 1 marker black, 157/158 dark gray, 201/202 light gray, 254 white [others] |
| 8 | u32 | thickness | 400 ink, 200 lasso and link box | [verified values]; width in 1/100 page pixel [others; consistent: best-fit constant width 4.5 px, see 9.5] |
| 12 | u32 | unknown | 10 for all strokes; 812, 148, 0 on link boxes | [unknown] |
| 16 | u32 | unknown | 0; 32 on link boxes | [unknown] |
| 20 | u32 | unknown | 32 | [unknown]; "font height" [others] |
| 24 | i32 | unknown | -1; 0 on link boxes | [unknown] |
| 28 | u32 | page number, 1-based | 1, 2, 3 | [verified] |
| 32 | u32 | unknown | 0 | [unknown] |
| 36 | u32 | unknown | 0 | [unknown] |
| 40 | i32 | record class | 5000 ink, -2 eraser, -5 lasso, 0 link box | [verified]; also -1, -4 for other eraser modes [others] |
| 44 | u32 | layer | 0 | [others: 0 MAINLAYER, 1-3 LAYER1-3]; only 0 in samples |
| 48 | char[52] | kind name, NUL-padded | `others` (ink, eraser, lasso), `0000` (link box) | [verified]; `0001` filled rect, `straightLine` ruler line, `fiveStarsSignal` star mark [others] |
| 100 | 6 x i32 | bbox: left, top, center x, center y, right, bottom in page pixels | e.g. 165, 158, 173, 194, 180, 229 | [verified for ink: equals the outline ring extents within 1.75 px (median 1.0), center = midpoint rounded either way]; on link boxes the right/bottom values do not match the rect [unknown] |
| 124 | i32 | unknown | 2; -1 on link boxes | [unknown] |
| 128 | u32 | maxA (vertical digitizer range) | 15819 (also on link boxes) | [verified] |
| 132 | u32 | maxB (horizontal digitizer range) | 11864 | [verified] |
| 136 | char[52] | document kind, NUL-padded | `superNoteNote`; empty on link boxes | [verified] |
| 188 | u32 | unknown | 1 | [unknown] |
| 192 | 4 x u32 | PDF anchor (chapter, position, offset x, offset y) | 0 | [others: used in `.mark` files] |
| 208 | u32 | n of "disable areas" (24 bytes each) | 0 | count [verified 0]; entry size [others] |

**B. Points**: `u32` n, then n x (`u32` a, `u32` b) [verified].

**C. Pressure**: `u32` n (same n), then n x `u16` [verified]. Range in samples 156 to 3915, median 2268 [verified]; the full scale is likely 4095 [inferred]. On link boxes all 0.

**D. Tilt**: `u32` n, then n x (`i16`, `i16`) [verified layout]. Values are multiples of 100 between -4400 and 5200, often constant over a stroke (t1: (3000, 2700) for many strokes) [verified]. Pen tilt in hundredths of a degree is plausible [inferred]; others read it as a direction vector [others]. Exact meaning [unknown].

**E. Point flags**: `u32` n, then n x `u8`; always 1 in the samples [verified].

**F. Two arrays, empty in the samples**: `u32` n + n x 8 bytes (a point list, same shape as part B), then `u32` n + n x 4 bytes (`i32` gray values) [counts verified as 0; element sizes others].

**G. Status** `i32` [verified]: `0` = the stroke is visible; `-99` = the stroke was erased. Verified: t2 record 78 (ink, `-99`) has only 9% of its points on ink, all live ink records have 100%; content page 1 has 8 records with `-99` (0% to low coverage) and 5 eraser records that together list exactly these 8 ids (one eraser lists 4). Others report further codes (-16 lasso delete, -4 partially erased with fragments in later records, -3 moved by lasso, -2 whole page cleared) [others].

**H. Operation** `i32` [verified]: `0` on ink (`99` on 20 of content page 2's 48 ink records), `601` eraser, `604` lasso path, `500` link box. Others report 602, 603 (other eraser modes), 400 (heading box), 14/4/2 (operation applied by a lasso: delete, move, copy) [others].

**I. Id** `u32`: per-page record id, starting at 1 or 2 in the samples, strictly increasing, gaps possible [verified]. Erased strokes keep their id; other records refer to ids (part K).

**J (fixed)**: 40 bytes: `u32` 0, `u32` 0 (shift angles before/after a lasso move [others]), 4 x i32 `0,0,1,1` and 4 x i32 `0,0,1,1` (rects before/after a move, identity when never moved [others]) [verified constant in all 651 records].

**K. Referenced ids**: `u32` n, then n x `u32` ids [verified]:
- eraser records list the ids of the strokes they erased (t2 record 79 lists id 79 = record 78, the `-99` stroke) [verified];
- link boxes list the ids of the strokes that form the linked handwriting (content: first link box lists ids 16 to 25) [verified];
- empty on ink and lasso records [verified].

**L. Group fields**: 9 bytes, all 0 in the samples, then 1 byte render flag (0 on all ink of t1/t2, 1 on nearly all records of bike/content) [verified layout and values]; names "group number, nesting, end, render flag" [others]; meaning of the flag [unknown].

**M. Outline**: `u32` ring count m, then per ring `u32` k and k x (`f32` x, `f32` y) in page pixels [verified]. Ink records have 1 to 9 rings, erasers and lasso paths have 0, link boxes have 1 (the link rectangle as 5 points, closed) [verified]. Rings with fewer than 3 points occur [verified].

**N. Ring hierarchy**: `u32` m (same as the ring count), then m x 4 x `i32`: next ring, previous ring, first child, parent; `-1` = none [verified: consistent tree on all records; this is the contour hierarchy layout used by OpenCV]. Outer rings have parent -1, holes have their outer ring as parent [verified]. Outer rings and holes have opposite winding (641 outer rings one direction, 211 holes the other) [verified], so both nonzero and even-odd filling give the same shape.

**O. Fixed and counted part** (21 bytes plus an array in the samples) [byte values verified; field split others]:
- `i32` -1 (unknown) [verified constant];
- `f64` 1.0 (unknown, a scale is plausible) [verified constant];
- `u8` 0 [verified];
- `i32` rotation in degrees from a lasso rotate [others]; 0 in samples;
- `u32` n + n x 4 bytes (`i32` values, unknown) [others]; n = 0 in samples;
- `u32` page width 1404, `u32` page height 1872 [verified];
- `u32`: 0 on ink, 20000000 on link boxes (= `LINKSTYLE`) [verified];
- `u8` 0 [verified].
The samples cannot tell apart the order of the zero-valued `u8`, rotation and count fields; the order above is the one others report and gives the same total length.

**P. Strings and trailer**:
- `u32` length + bytes: `none` on ink; on link boxes a comma-separated list of base64 items, see 9.6 [verified].
- `u32` length + bytes: `none` [verified].
- `u32` length + bytes: empty (length 0) [verified].
- `u32` 3 [verified constant].
- `u32` n, then n x 8 bytes (`f32` pairs, "marker fill direction" [others]) [verified: n is 0 in bike and older content records, 2 with zero values in t1, t2 and newer content records].
- 20260016 only: 4 more zero bytes [verified: t1, t2].

A parser should read records strictly by the record length (9.1) and treat anything after part O that does not parse as raw bytes [inferred]. Every count field must be honored with its element size, even where the samples only show 0: disable areas 24 bytes (A at 208), points 8 (B, F first), pressure 2 (C), tilt 4 (D), flags 1 (E), gray values 4 (F second), ids 4 (K), ring points 8 (M), hierarchy entries 16 (N), part O array 4, trailer pairs 8 (P). Only the head (212 bytes including the A count), the 40-byte span J, the 10-byte span L and the scalar fields of O and P are fixed [verified layout].

### 9.4 Units [verified]

- Point coordinates: digitizer units, 8.45 per page pixel on N6/A6X.
- Outline and bbox: page pixels.
- ink.bink (8.3): millimeters; ratio to page pixels 0.0845, i.e. 300 dpi.
- Thickness: 400 for all ink in the samples. 400 / 100 = 4 px [others]; 0.4 mm at 300 dpi = 4.7 px [inferred alternative]. Both fit the measurement in 9.5.

### 9.5 Drawing strokes as vectors (for an SVG converter)

Measured on t2 against the decoded MAINLAYER (ink = any non-transparent code), page 1404 x 1872 [verified]:

| Method | Precision | Recall | IoU |
|---|---|---|---|
| fill the outline rings (9.3 M, N) of all live ink records | 0.985 | 0.833 | 0.823 |
| centerline (9.3 B) as polyline, round caps and joins, constant width 4 px | 0.993 | 0.766 | 0.762 |
| same, width 4.5 px (4x supersampled, coverage >= 25%) | 0.969 | 0.984 | 0.954 |
| same, width 5 px | 0.891 | 0.999 | 0.890 |
| width 3 + 3 * pressure / 4095 px | 0.940 | 0.980 | 0.922 |

Outline filling on the other pages: t1 IoU 0.840, bike 0.754/0.769/0.734, content page 2 0.787 [verified]. Content page 1 is lower (0.572) because the device-drawn link icons are only in the bitmap [verified].

Reading: the outline rings cover the dark core of the drawn ink. Against a mask of only black and levels below 0x80, ring filling reaches recall 0.94/0.92 and IoU 0.85/0.86 on t1/t2 [verified]; the light anti-aliasing fringe lies outside the rings. For the needle-point pen used in all samples, the centerline at about 4.5 px matches the bitmap best (IoU 0.954) [verified]. Other pens (ink pen, calligraphy) vary their width with pressure and tilt, which only the outline rings capture [others]. Recommendation for an SVG converter: use the outline rings as the shape of each live ink record (they hold the device's own shape for every pen type) and offer the centerline at `thickness / 100` px as the fallback for records without rings; for needle-point strokes both are good, the centerline slightly closer to the device bitmap [inferred from the measurements].

Records to skip when drawing [verified unless marked]:

- status (part G) not 0 (erased);
- tool 3 or color 255 (eraser path);
- record class -5 or tool 4 (lasso path);
- kind name `0000` (link box; the rectangle is not drawn by the device);
- class 0 records with kind `0001` are filled rectangles (heading backgrounds) and `straightLine` records are lines between two points [others].

Erasing in the samples always removed whole strokes; partial erasing is recorded with fragment records [others].

Colors: ink color 0 = black in all samples [verified]; draw color c as gray c (0, 157/158, 201/202, 254) [others].

Layers: a stroke's layer is the field at offset 44 [others]; all samples are MAINLAYER-only, so which bitmap layer a stroke belongs to could not be tested [unknown].

### 9.6 Link boxes in the stroke data [verified]

Creating a link can write a lasso path record (tool 4, class -5, operation 604) followed by a link-box record (tool 0, class 0, kind `0000`, operation 500) [verified: content records 33/34, 45/46, 64/65, 74/75; the link boxes 25 and 26 have no lasso record before them]. The link-box record:

- lists the ids of the linked strokes (part K);
- has one outline ring equal to the `LINKRECT` rectangle;
- carries `LINKSTYLE` (20000000) as the `u32` in part O;
- has as first string a comma-separated list of base64 items, terminated by a trailing comma (so splitting yields one empty last item to drop; erratum, 2026-10-06), repeating the link block: `LINKTYPE`, `LINKINOUT`, `LINKSTYLE`, `LINKTIMESTAMP`, `LINKRECT`, `LINKRECTORI`, decoded `LINKFILE` path, `LINKFILEID`, `PAGEID`, `OBJPAGE`, `FONTSIZE`, `FONTPATH`, `FULLTEXT`, `SHOWTEXT` (14 items; newer links have 19, the 5 extra items being `0, 1, 1, 0, none`, which include the `ANTI`/`BOLD`/`ITALIC` flags) [verified items 1-14; mapping of the extra 5 inferred].

### 9.7 Is the vector data complete for a page?

- `TOTALPATH:0`: no vector data; only the bitmap exists [verified: t1 and t2 in their early saves, while the bitmap already held ink].
- Present: it is complete for the save in which it was written. Because the bitmap is written one save earlier (2.), a file captured in between has vectors older than the bitmap [verified pattern]. A cheap check: the TOTALPATH block address is greater than the address of the MAINLAYER bitmap block in every final state of the samples [verified]; in the intermediate saves it was smaller [verified]. A stronger check: rasterize the live outlines and compare with the bitmap ink (IoU around 0.75 to 0.84 for complete data on these devices, 9.5) [inferred threshold].
- Device-drawn content without strokes: link icons (verified), text boxes and digests (`DISABLE` regions) [others], and anything pasted as an image [inferred]. These exist only in the bitmap.
- Erased strokes and eraser/lasso paths stay in TOTALPATH (append-only per page until compaction) [verified]. The firmware removes fully erased strokes at some point [others]; bike (compacted) has no erased records [verified].

## 10. Robustness rules for a parser

Out-of-bounds risks and invariants [inferred unless marked]:

1. File shorter than 32 bytes, missing `tail` before the last 4 bytes, or footer address outside `[24, len-8)`: reject or fall back to a sequential walk (2.) to find the last valid footer.
2. Every address: check `addr + 4 <= len` and `addr + 4 + N <= len` before reading a block. Address 0 means absent; do not read offset 0.
3. Metadata: decode as UTF-8 with replacement on error; never trust a value to be numeric. Parse integers with overflow checks; addresses must fit `u32`.
4. Cycles cannot occur in a well-formed file (addresses point to blocks, never back to the footer), but a corrupt file can point a page key at the footer itself; detect re-visits.
5. RLE: stop when W*H pixels are produced and report extra input; report short output instead of panicking. A run must not write past W*H. A `len` of `0xFF` after a high part, an odd stream length, or a high part at the end are format violations in the samples' terms; decide per policy (strict: error; lenient: reference behavior).
6. Page size: determine from `APPLY_EQUIPMENT` and orientation before decoding; an unknown device code should fall back to 1404 x 1872 and be reported.
7. TOTALPATH: check `R` against the block length (each record needs at least 4 bytes); check each record length against the remaining bytes; inside a record check every count against the remaining record bytes (`n * size`) before allocating. Point counts in parts B to E must be equal [verified invariant]; ring count in N must equal the ring count in M [verified invariant].
8. Digitizer range fields of 0 must not be used for scaling; others report one non-stroke record with a range of 120 [others]. Skip points of records whose range is implausible.
9. RECOGNFILE: treat as an untrusted ZIP (size limits, path traversal in entry names).
10. RECOGNTEXT: base64 then JSON; both can fail.
11. Unknown signature: parse with these rules, mark as unrecognized. Unknown `LAYERPROTOCOL`: expose the raw block.
12. Unknown keys anywhere: keep them as raw strings.

Plausible invariants worth checking [verified on samples]: `tail` before each footer address; the footer address points to a block that ends exactly 8 bytes before end of file; footer `FILE_FEATURE` points to a block whose first key is `MODULE_LABEL` or `FILE_TYPE`; `LAYERSEQ` lists only layers with a non-zero address; each layer bitmap decodes to exactly W*H; record page number (9.3 A offset 28) equals the page.

## 11. Test vectors

All values computed by an independent decoder written from this document, and cross-checked against supernotelib 0.7.3 (layer decoding with the X2 table, raw TOTALPATH and RECOGNFILE blocks): all hashes and counts agree [verified].

Definitions:
- "codes" = the decoded RLE output, one byte per pixel, the raw code values, row-major.
- "gray" = codes mapped with the X2 table of 5.3 (`0x63` -> 0x30, `0x64` -> 0x50, `0x65` -> 0xFE, `0x66` -> 0, `0x61` -> 0), transparent -> 255.
- "opaque" = pixels whose code is not `0x62`.

### 11.1 t1 (`test1.note`, 186649 bytes)

| Item | Value |
|---|---|
| footer address | 186563 |
| header address | 3194 |
| page 1 block | 185284 |
| MAINLAYER block / bitmap | 48971 / 5037, 43930 bytes, opaque 34435 |
| MAINLAYER sha256(codes) | `5d901d57bcbdd9660225cc667a397da49b071942451c401f6139d8d9e0b12510` |
| MAINLAYER sha256(gray) | `56aeaf16e108c6f6ada26498a5eb0256792a9d403ed3e0f490a74ab3fa8bbb57` |
| BGLAYER block / bitmap | 1255 / 330, 462 bytes, opaque 21168 |
| BGLAYER sha256(codes) | `04238c0017e9a91e915a3d8ce2f5b78779e34d45d7e6fbe8a740be76b27761d4` |
| BGLAYER sha256(gray) | `2b7209c3f1c749fa26a43d54acdf52b11934d23554f249c6e9bbfe3125ba3658` |
| page composite with template, gray, canvas 255, sha256 | `2846069b5cc07c9f66ec37b56b6fe6e4e8143cb5b07a50893cfa36ba01bf5da8` |
| page composite without template | same as MAINLAYER gray |
| TOTALPATH | 50458, 104841 bytes, 72 records, all (tool 10, class 5000, status 0) |
| TOTALPATH sha256 | `2488505f0e3814f40e679cbf4b50349e2b30158b6fd48bc0bd6cf78d400de162` |
| RECOGNFILE | 156663, 27252 bytes, ZIP with 7 entries; `ink.bink` 72 strokes |
| first stroke | 42 points, first point (a=1404, b=10347) -> pixel (179.5, 166.1); bbox 165,158,173,194,180,229; 7-point outline ring |
| MAINLAYER code histogram | `0x61` 22147, `0x62` 2593853, `0x66` 2, plus anti-aliasing levels (e.g. `0xBF` 1586, `0x3F` 1322, `0x7F` 1303) |

### 11.2 t2 (`test2.note`, 240988 bytes)

| Item | Value |
|---|---|
| footer / header / page 1 | 240904 / 24 / 239630 |
| MAINLAYER block / bitmap | 81084 / 2704, 78376 bytes, opaque 65293 |
| MAINLAYER sha256(codes) | `5ccef18d500034f6d7b1b2a65bc2c9af865c17926eed4be5ef72f68200f59b88` |
| MAINLAYER sha256(gray) | `2eab8aa3128e701e2639129d755cb1c73652cce4fb5df84d46d773a52cb4714c` |
| BGLAYER | 1255 / 330, identical to t1 |
| composite with template sha256 | `a17bba7ecbf7bb318a5e291d801a5c5855aca347ae44df7f0413048c04abc56b` |
| TOTALPATH | 82569, 157057 bytes, 84 records: 82 live ink, 1 erased ink (record 78, id 79, 290 points), 1 eraser (record 79, id 80, refers to id 79) |
| TOTALPATH sha256 | `93bb6a117f2921c09ece7cfa8fa6b407be2ce78412084c09122ad4524c061f97` |
| vector check | best centerline width 4.5 px: IoU 0.954 against MAINLAYER ink |

### 11.3 bike (local only, 1091064 bytes)

| Page | Page block | MAINLAYER bitmap (bytes, opaque, sha256 gray) | TOTALPATH (address, records, sha256) |
|---|---|---|---|
| 1 | 544370 | 1233 (108918, 86472, `be9b49f0bd88405f67d067a8293e1f3036f8199bf3bf02645bddba1a7bd4414a`) | 110416, 190, `9ac691b048787dd6a74fc825ea82ea32c7dd72d6947f2a2fe1e26887dcce9522` |
| 2 | 958869 | 545619 (83966, 66035, `c80dc3bd7346d0f41f4980eb17e93c0abebd4bd15854ef9937867170261f9af8`) | 629852, 129, `00edacc7c17f0d0c5c5655d2ff888caaaf8dede85be7580c2ddc560494fbe1c0` |
| 3 | 1089668 | 960118 (23920, 18351, `7363342760ba0121ca5c50ea2ad30717cfd5e13a09b5577acb9adf17d50cb616`) | 984305, 52, `0b468183c96857f7a3304b1cd262683d0218587514462e6d7a6b4611a32587e2` |

BGLAYER bitmap 439 on all pages, identical bytes to t1's 330. `FILE_ID` `F20240708174639550429bHksqaP2wKQ8`. Record page number field reads 1, 2, 3 respectively.

### 11.4 content (local only, 1681602 bytes)

| Page | Page block | MAINLAYER bitmap (bytes, opaque, sha256 gray) | TOTALPATH (address, records, sha256) |
|---|---|---|---|
| 1 | 1679987 | 1431290 (41256, 40903, `2f951b16a780880d67b118b4deea9d91dce9a3b6e9246e270fff77856a08a2d1`) | 1474300, 76: 53 live ink, 8 erased ink, 5 eraser, 4 lasso, 6 link box; `77db68565a3e55af021717bcd57af973dd61c04873d8e3727232daebe4490689` |
| 2 | 283760 | 148564 (29070, 21879, `f1dfb516dced09dba4d53f0847bd600c693846f97d365da0ab838e02cf4579bf`) | 177902, 48, `becab8806026defc77554bc637ca687b99d3b2b9cc6a09a46fb8c1745fa06121` |

Links: 6 blocks at 452, 843, 1252, 1638, 500079, 1180240, all `LINKTYPE:1`, `LINKINOUT:0`, page 1; bitmaps at 440, 831, 1242, 1626, 496157, 1174576. The block at 1252 targets bike (7.3).

### 11.5 Golden images

Rendering per 6.1 reproduces all of `tests/fixtures/golden/test{1,2}-p1-{bg,nobg}.png` and `tests/fixtures/local/golden/{bike-p1..p3,content-p1..p2}-{bg,nobg}.png` with 0 differing pixels [verified].

## 12. Unknowns and open items

Raw-only fields (expose as raw data): header `MODULE_LABEL`, `FILE_PARSE_TYPE`, `RATTA_ETMD`, `APP_VERSION`, `PDFSTYLE`, `PDFSTYLEMD5`, `STYLEUSAGETYPE`, `HORIZONTAL_CHECK`, `IS_OLD_APPLY_EQUIPMENT`; page `THUMBNAILTYPE`, `RECOGNTYPE`, `RECOGNLANGUAGE`, `EXTERNALLINKINFO`, `IDTABLE` (both address-shaped, 0 in samples); layer `LAYERPATH`, `LAYERVECTORGRAPH`, `LAYERRECOGN` (address-shaped, 0 in samples); link `LINKSTYLE`, `FONTSIZE`, `FONTPATH`, `FULLTEXT`, `SHOWTEXT`, `ANTI`, `BOLD`, `ITALIC`; stroke head offsets 12, 16, 20, 24, 32, 36, 124, 188; tilt semantics; render flag; trailer values; `page.bdom`, `style.css`, the `ink.bink` element table.

Not present in the samples: titles, keywords, stars, covers, thumbnails, custom (PNG) or PDF templates, landscape pages, layers 1-3, `SN_ASA_COMPRESS`, X-table files (versions below 20230015), `.mark` files, RECOGNTEXT, N5 (1920 x 2560) files, other pens than needle point, other colors than black, lasso move/delete, partial erase, text boxes and digests. Each of these needs a sample before its rules can be verified.

## Appendix A. Data model for a complete parser

Entities and fields a parser should expose (no API prescribed). Section references point to the defining rules.

| Entity | Fields | Sections |
|---|---|---|
| File | file type, signature string and version number, recognized-version flag, file length, current footer address, `FILE_ID` with decoded creation time, device code, page size, all header key/values (raw and typed), real-time recognition flag and language, `DIRTY`, history of saves (list of footer addresses) | 1, 2, 3.1, 4.1, 4.2, 4.6 |
| Raw block | address, length, payload bytes, kind (metadata/binary), reachable flag | 2, 3 |
| Save (history) | footer address, footer map, header address, page addresses | 2 |
| Page | index (from key), `PAGEID` and creation time, style name and md5, orientation, size, `LAYERSEQ`, `LAYERINFO` entries, layer list, stroke list, recognition status fields, recognition archive, recognition text, all other page keys raw, vector freshness indicator | 4.3, 4.4, 9.7, 8 |
| Layer | slot (MAIN, 1-3, BG), block address, type, protocol, name, bitmap address, path/vectorgraph/recogn addresses, visibility and display name from LAYERINFO, decoded codes buffer | 4.4, 4.5, 5 |
| Bitmap | protocol, raw bytes, width, height, decoded codes, gray image per code table, opaque count | 5 |
| Template | style name, footer address, bitmap, list of pages using it | 4.1, 5.4 |
| Page image | composite with/without template, golden-compatible and device-compatible variants | 6 |
| Stroke record | index, id, tool, color, thickness, record class, layer, kind name, document kind, page number, bbox, digitizer ranges, points (raw and in pixels), pressure, tilt, point flags, status (live/erased/other code), operation code, referenced ids, render flag, outline rings with hierarchy, page size fields, strings, trailer raw, all unknown fields raw | 9 |
| Link | footer key with decoded page and position, all link keys, decoded target path or URL, target file id, target page id, kind, direction, rect, timestamp, bitmap, link-box stroke record and linked stroke ids | 4.1, 7.3, 9.6 |
| Title | footer key with page and position, rect, original rect, level, sequence number, style (decoded background and text gray), protocol, bitmap | 7.1 |
| Keyword | footer key with page and position, page, rect, original rect, sequence number, text (from `KEYWORDSITE` and inline), length | 7.2 |
| Cover | footer key (`COVER_0/1/2`), address, raw bytes | 4.1, 6.3 |
| Recognition | RECOGNTEXT decoded JSON (elements, labels, word boxes), RECOGNFILE archive entries, `ink.bink` strokes with timestamps and mm coordinates, raw `page.bdom`/`style.css` | 8 |

## Appendix B. Coverage compared with other parsers

Projects surveyed (feature inventory only): supernotelib (reference, Python), supernote-typescript (TypeScript, also used by supernote-obsidian-plugin), calamus (Rust, format notes plus bitmap and stroke rendering), sntool and snorg (Go; snorg uses sntool), sn2md (Python, uses supernotelib; its templates show the fields users want), supernote-obsidian-plugin, allenporter/supernote (Python server, own copy of a supernotelib-style parser), ultrabridge (Go server; reads keywords, titles and RECOGNTEXT, writes RECOGNTEXT).

"Required" = at least one other parser extracts it; notedown's parser must match it. "Beyond baseline" = found only in this work.

| Data kind | supernotelib | supernote-typescript | calamus | sntool/snorg | sn2md | allenporter | ultrabridge | This doc | Status |
|---|---|---|---|---|---|---|---|---|---|
| envelope, signature, footer/header/page/layer metadata | yes | yes | yes | yes | via lib | yes | partial | 1-4 verified | required |
| append-only history, stale block handling | current only | current only | current only | current only | via lib | current only | - | 2 verified, history exposed | required (current state); history beyond baseline |
| RATTA_RLE, X and X2 tables, anti-aliasing levels | yes | yes | yes | yes | via lib | yes | - | 5.2, 5.3 verified | required |
| SN_ASA_COMPRESS (original A5) | yes | - | yes | yes | via lib | yes | - | 5.1 reference | required, not in samples |
| PNG custom templates | yes | yes | yes | yes | via lib | yes | - | 5.1 reference | required, not in samples |
| layer visibility from LAYERINFO, LAYERSEQ order | yes | yes | yes | yes | via lib | yes | - | 4.4, 6.1 verified | required |
| landscape orientation | yes | yes | - | yes | via lib | yes | - | 4.6 reference | required, not in samples |
| page rendering (PNG) | yes | yes | yes | yes | via lib | yes | - | 6 verified pixel-exact | required |
| SVG by tracing bitmaps | yes | yes | yes | yes | via lib | yes | - | (renderer concern) | required (output feature) |
| keywords (text, page, rect, bitmap) | yes | yes | - | yes | yes | yes | yes | 7.2 reference | required, not in samples |
| titles (rect, bitmap, level, seq no) | yes (rect, bitmap) | yes | - | yes | yes (level) | yes | yes | 7.1 reference | required, not in samples |
| title style (`TITLESTYLE`) | - | yes | - | - | - | - | - | 7.1 others | required, not in samples |
| links (type, direction, path/URL, file id, page id, rect, timestamp) | yes | yes | - | yes | yes | yes | - | 7.3 verified | required |
| link `OBJPAGE` target page | - | yes | - | yes | - | - | - | 7.3 value verified | required |
| link bitmap | stored | yes | - | yes | - | yes | - | 6.3 verified (size = rect) | required |
| stars (`FIVESTAR`, star stroke kind) | - | yes | - | yes | - | - | - | 4.3, 9.3 others | required as presence flag plus raw value; value layout undocumented everywhere |
| cover image | yes | yes | - | yes | - | yes | - | 4.1, 6.3 reference | required as raw bytes (no parser decodes it); not in samples |
| real-time recognition text (RECOGNTEXT) | yes | yes (with word positions) | - | yes | via lib | yes | yes | 8.2 reference/others | required, not in samples; word-box units unverified |
| RECOGNFILE archive, `ink.bink` strokes and timestamps | - | yes | - | - | - | - | - | 8.3 verified | required |
| TOTALPATH container and records | stored raw | yes | yes | - | - | - | - | 9.1-9.3 verified | required |
| stroke points to pixel transform | - | yes | yes | - | - | - | - | 9.2 verified | required |
| pen tool, color, thickness | - | yes | yes | - | - | - | - | 9.3 verified values | required |
| erased-stroke status, eraser and lasso records | - | yes | partial | - | - | - | - | 9.3 G, H verified | required |
| outline rings (device-rendered stroke shape) | - | yes | parsed | - | - | - | - | 9.3 M verified | required |
| outline ring hierarchy (holes) | - | - | parsed as unknown | - | - | - | - | 9.3 N verified | beyond baseline |
| eraser and link records referencing stroke ids | - | - | - | - | - | - | - | 9.3 K verified | beyond baseline |
| link-box stroke record and its embedded link fields | - | excluded only | - | - | - | - | - | 9.6 verified | beyond baseline |
| stroke layer field | - | yes | yes | - | - | - | - | 9.3 others | required, not testable on samples |
| text boxes and digests (`DISABLE`, `PAGETEXTBOX`) | - | yes | - | - | - | - | - | 4.3 others | required, not in samples |
| heading rects, ruler lines, partial-erase fragments | - | yes | - | - | - | - | - | 9.5 others | required, not in samples |
| vector freshness versus bitmap | - | heuristic | - | - | - | - | - | 9.7 verified pattern | beyond baseline |
| device export compositing (multiply, black = 1) | - | - | - | - | - | - | - | 6.2 verified | beyond baseline |
| `0x63`/`0x64` as anti-aliasing levels in X2 files | - | - | - | - | - | - | - | 5.3 inferred | beyond baseline |
| per-save history of headers and pages | - | - | - | - | - | - | - | 2 verified | beyond baseline |
| `.mark` files | yes | partial | doc only | - | - | - | yes (PDF side) | 1 others | required, not in samples |
| `HIGHLIGHTINFO` | - | - | doc only | - | - | - | - | 4.2 value verified | raw only |
| writing RECOGNTEXT into files | - | - | - | - | - | - | yes | out of scope (read-only) | not required for a reader |
