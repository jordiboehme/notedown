# Supernote `.note` file format

This document describes the binary format of Supernote `.note` notebook files as far as it could be established from four sample files. It is written so that a parser can be implemented from this text and the samples alone. The open-source Python package supernotelib (0.7.3) was used as a reference for facts about the format; nothing in this document is derived from its code structure. A second source is Ratta's own server-side note renderer (mark [ratta-notelib]); its facts were taken from symbol names, strings and single disassembled functions, no code was copied.

## 0. Conventions

### Evidence marks

Every statement carries one of these marks:

| Mark | Meaning |
|---|---|
| [verified] | Checked on the sample files with an independent decoder written from this text. |
| [reference] | Taken from the reference implementation only; not observed in the samples. |
| [others] | Reported by other community parsers (see Appendix B); not observed in the samples. |
| [ratta-notelib] | Seen in Ratta's server-side note renderer notelib 6.9.3 (June 2025, the `noteServer` binary of the private cloud). It renders note and mark pages to PNG/PDF only: it never draws vectors, recognition, titles, keywords or text boxes, and it does not know `SN_FILE_VER_20260016`. A key it does not read is not proof that the key is meaningless. Evidence is cited by function or table name. |
| [inferred] | Reasoning from the samples that is consistent but not proven. |
| [unknown] | Present in the samples, meaning not established. A parser should expose it as raw data. |

### Sample files

| Short name | Path | Signature | Device code | Pages | Notes |
|---|---|---|---|---|---|
| t1 | `tests/fixtures/notes/test1.note` | `SN_FILE_VER_20260016` | `N6` | 1 | real-time recognition note, ruled template, 8 saves in the file |
| t2 | `tests/fixtures/notes/test2.note` | `SN_FILE_VER_20260016` | `N6` | 1 | two drawn diagrams, ruled template, one erased stroke |
| bike | `tests/fixtures/local/bike.note` (local only) | `SN_FILE_VER_20230015` | `N6` | 3 | compacted file, no stale data |
| content | `tests/fixtures/local/content.note` (local only) | `SN_FILE_VER_20230015` | `A6X` | 2 | 6 links to other notes, 15 saves, erased strokes, lasso records |
| bike-digest | `tests/fixtures/local/bike-digest.note` (local only) | `SN_FILE_VER_20260016` | `N6` | 3 | bike edited on firmware Chauvet 3.29.42: a handwritten word on page 3 turned into a text box that is a note digest; compacted |

The handwritten content of the samples is private. This document describes structure only. Worked examples use t1 and t2 where possible; for bike and content only counts, addresses and hashes are given.

### Byte order and types

All integers are little-endian [verified]. `u8`, `u16`, `u32`, `i16`, `i32`, `u64` have their usual meaning; `f32` and `f64` are IEEE 754 little-endian floats [verified]. An "address" is a `u32` absolute byte offset from the start of the file [verified].

## 1. File envelope

| Offset | Size | Content |
|---|---|---|
| 0 | 4 | File type, ASCII. `note` in all samples [verified]. `mark` for PDF annotation files [others; ratta-notelib]. Ratta's loader refuses anything else on this path (`fetchFileVertion`, `CheckIfA6X`); a file starting with `SN_FILE` goes to the original A5 path [ratta-notelib]. |
| 4 | 20 | Signature, ASCII `SN_FILE_VER_` followed by 8 decimal digits [verified]. |
| 24 | ... | First block (in all samples a header block) [verified]. |
| end-8 | 4 | ASCII `tail` [verified]. Ratta does not check it (`fetchModuleIndexSite` reads only the address) [ratta-notelib]. |
| end-4 | 4 | `u32` address of the current footer block [verified]. |

### Known signatures

| Signature | Status |
|---|---|
| `SN_FILE_VER_20230015` | [verified] bike, content |
| `SN_FILE_VER_20260016` | [verified] t1, t2 |
| `SN_FILE_VER_20200001`, `...20200005`, `...20200006`, `...20200007`, `...20200008`, `...20210009`, `...20210010`, `...20220011`, `...20220013`, `...20230014` | [reference] older firmware generations |
| `SN_FILE_ASA_20190529` | [reference] original A5. The signature starts at offset 0 (it replaces the file type field), there are no layers, pages use `DATA`/`PROTOCOL` keys and the footer repeats a plain `PAGE` key. |

The digits are a date-like version number (`YYYY` + 4-digit sequence) [inferred]. A version of `20230015` or higher selects the "X2" color code table for RATTA_RLE bitmaps (section 5.3) [reference; consistent with all samples].

Version gate in Ratta's renderer [ratta-notelib]: the first 24 bytes (`note` + signature, `fetchFileHeader`) are compared as a string with `noteSN_FILE_VER_20230015` (`markSN_FILE_VER_20230015` for marks). Greater or equal selects the "15" decoder and table (`fetchNoteRLEData_15`), smaller the old decoder (`fetchNoteRLEDataFromFile`), see 5.2 and 5.3. There is no list of known versions: `20260016` and any later signature take the "15" path. A second threshold exists: a signature below `SN_FILE_VER_20210009` sets a background flag in the layer loop; its effect was not traced [ratta-notelib].

### Differences between 20230015 and 20260016 in practice

No difference in container structure, block encoding, metadata syntax, RLE or stroke layout was found [verified]. Differences seen:

- Stroke records written by 20260016 firmware have a trailer 4 bytes longer (section 9.3, part P). This depends on the firmware that wrote the TOTALPATH block, not on the file signature: bike-digest (20260016) keeps the byte-identical old TOTALPATH blocks of pages 1 and 2 with the short trailer, while the rewritten page 3 block has the long trailer for all its records, old and new [verified].
- Some metadata keys appear only in blocks written by newer firmware: page keys `PAGETEXTBOX`, `DISABLE`; link keys `ANTI`, `BOLD`, `ITALIC`. They depend on the firmware that wrote the block, not on the file signature: content (20230015) has them in blocks written in 2026 and lacks them in older blocks of the same file [verified].
- Upgrade on edit [verified: bike to bike-digest]: when firmware 3.29.42 edited a 20230015 file, it wrote the signature `SN_FILE_VER_20260016`, rewrote every page block (adding `PAGETEXTBOX` and `DISABLE`, all other keys and the `LAYERINFO` value unchanged), kept the header block byte-identical, kept the bitmaps and TOTALPATH blocks of unedited pages byte-identical, and rewrote only the edited page's MAINLAYER bitmap and TOTALPATH. It saved the result compacted (one save marker, no stale blocks). A reader must therefore expect mixed-generation content inside one file and decide per block, never per signature.
- The first header written at creation can be a short variant without `MODULE_LABEL`, `APP_VERSION`, `PDFSTYLE`, `PDFSTYLEMD5`, `STYLEUSAGETYPE`, `HIGHLIGHTINFO` (t1 at 24, t2 at 24, 302 bytes); later saves write the full header (t1 at 3194, 402 bytes) [verified].

### Unknown signatures

A reader should accept any `SN_FILE_VER_` + 8 digits and parse it with these rules, flagging the version as unrecognized rather than failing [inferred]. The block structure has been stable across all versions the reference knows [reference]. Ratta's renderer does the same: it parses any signature and only compares it with the two thresholds above [ratta-notelib].

## 2. Block chain and append-only saves

### Blocks

Everything after the 24-byte prefix is a sequence of blocks, each:

| Size | Content |
|---|---|
| 4 | `u32` length N |
| N | payload |

A block is referenced by the address of its length field [verified]. Ratta treats a length that reaches past the end of the file as an error ("content size is bigger than file size") [ratta-notelib]. There are two payload kinds: metadata blocks (UTF-8 text of `<KEY:VALUE>` pairs, section 3) and binary blocks (bitmaps, stroke data, ZIP archives) [verified].

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
2. The 4 bytes before it should be `tail` [verified]. Ratta's renderer does not check this [ratta-notelib], so a missing `tail` should be a warning, not an error [inferred].
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

Ratta's parser is stricter [ratta-notelib]: it treats the payload as a C string (a NUL byte ends it), splits it on `>`, then splits each item on `:`. An item that does not give exactly two parts logs "module format error." and is skipped; parsing goes on. So an item without `:` and an item whose value contains `:` are both dropped. The key is the first part without the leading `<`. Items are kept in file order with repeats, and a lookup takes the first match. The rule above (value up to `>`) is more lenient; a parser that follows it should at least flag items that Ratta would drop [inferred]. No such item occurs in the samples [verified: no value contains `:`].

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
| `PAGE1`, `PAGE2`, ... | address | page blocks, 1-based page number in the key, no zero padding | [verified]; Ratta counts pages as the number of footer keys starting with `PAGE` and looks up page n as `PAGE` + decimal n (`getTotalPageNum`) [ratta-notelib]; `.mark` files may have gaps (6.4) |
| `FILE_FEATURE` | address | header block | [verified] |
| `DIRTY` | integer | 0 in bike (compacted), 1 in t1/t2, 6 in content; rises with unsaved edits | [verified values], meaning [others: count of modifications since last compaction] |
| `COVER_0` | `0` | no cover | [verified] |
| `COVER_1`, `COVER_2` | address | cover image, a PNG (6.3); `COVER_2` preferred over `COVER_1` | [reference]; PNG [ratta-notelib] |
| `COVER_3` | address | cover image, a JPEG (6.3) | [ratta-notelib] |
| `STYLE_<stylename>` | address | template bitmap per style name, e.g. `STYLE_style_8mm_ruled_line:330`, `STYLE_style_white:905` | [verified] |
| `STYLE_user_<name><md5>` | address | custom template (PNG), md5 appended to the name | [reference] |
| `TITLE_<page4><top4><left4>` | address | title block | [reference]; shape [others] |
| `KEYWORD_<page4><top4>` | address | keyword block | [reference]; shape [others] |
| `LINKO_<page4><top4><left4><height4><width4>` | address | outgoing link block | [verified]; Ratta reads the page as the 4 characters after the prefix [ratta-notelib] |
| `LINKI_...` | address | incoming link block, same block keys as 7.3 [inferred] | prefix [ratta-notelib]; not in the samples [verified]; shape of the rest [inferred: like `LINKO_`] |

The position digits in `LINKO_` keys equal the link's `LINKRECT` (section 7.3) [verified on all 6 links]: `LINKO_00010246081200840537` = page 0001, top 0246, left 0812, height 0084, width 0537 for `LINKRECT:812,246,537,84`. The page number for titles, keywords and links must be taken from the key [verified for links; reference for titles; others report `KEYWORDPAGE` can be wrong].

t1 and t2 carry no `COVER_` key at all [verified]; bike and content carry `COVER_0:0` [verified]. Ratta expects at most one cover key ("vecSite more than 1") [ratta-notelib]. Title and keyword footer keys are only sorted by Ratta's renderer, never drawn [ratta-notelib].

### 4.2 Header keys

| Key | Sample values | Meaning | Mark |
|---|---|---|---|
| `MODULE_LABEL` | `none` (t1), `SNFILE_FEATURE` (bike, content) | unknown | [unknown] |
| `FILE_TYPE` | `NOTE` | file kind | [verified] |
| `APPLY_EQUIPMENT` | `N6`, `A6X` | device model code, selects page size (4.6) | [verified]; Ratta knows `A6X`, `A5X`, `N5`, `N6` and treats anything else as `N6` [ratta-notelib] |
| `FINALOPERATION_PAGE` | `1`, `3`, `2` | last page edited (1-based) | [inferred: equals a valid page] |
| `FINALOPERATION_LAYER` | `1` | last layer edited | [inferred] |
| `DEVICE_DPI` | `0` | 0 on these devices | [verified value], meaning [others] |
| `SOFT_DPI` | `0` | as above | [verified value] |
| `FILE_PARSE_TYPE` | `0` | unknown | [unknown] |
| `RATTA_ETMD` | `0` | unknown | [unknown] |
| `APP_VERSION` | `0` | unknown | [unknown]; Ratta's key list spells it `APP_VERTION` and has no `APP_VERSION`, so it never reads the samples' value [ratta-notelib; samples spell `APP_VERSION`, verified]. Accept both spellings [inferred] |
| `ORIGINAL_STYLE` | absent | unknown | key [ratta-notelib]; not in the samples [verified] |
| `COVER` | absent | unknown, maybe the cover kind | key [ratta-notelib]; not in the samples [verified]; meaning [unknown] |
| `FILE_ID` | see 3.1 | file identity, target of links | [verified] |
| `FILE_RECOGN_TYPE` | `0`, `1` | `1` = real-time recognition note | [verified with t1 history], meaning [reference] |
| `FILE_RECOGN_LANGUAGE` | `none`, `en_US` | recognition language | [verified] |
| `PDFSTYLE` | `none` | PDF used as template | [unknown] |
| `PDFSTYLEMD5` | `0` | hash of that PDF | [unknown] |
| `STYLEUSAGETYPE` | `0` | unknown | [unknown] |
| `HIGHLIGHTINFO` | `0` | address of highlight data: a block of `u32` length + base64 text | value [verified]; block shape [ratta-notelib, `loadHighlightInfo`, content not parsed]; JSON inside [others] |
| `HORIZONTAL_CHECK` | `0` | unknown | [unknown] |
| `IS_OLD_APPLY_EQUIPMENT` | `1` (t1, t2), `0` (bike, content) | unknown | [unknown] |
| `ANTIALIASING_CONVERT` | `2` | anti-aliased bitmaps (section 5.3) | value [verified], meaning [inferred] |

### 4.3 Page keys

| Key | Sample values | Meaning | Mark |
|---|---|---|---|
| `PAGESTYLE` | `style_8mm_ruled_line` | template name; `style_white` = blank; `user_...` = custom PNG | [verified] for the first two, [reference] for `user_` |
| `PAGESTYLEMD5` | `0` | hash of custom template, `0` = none | [verified value] |
| `LAYERINFO` | JSON, see 4.4 | layer names, visibility, flags | [verified] |
| `LAYERSEQ` | `MAINLAYER,BGLAYER` | layer stacking order, first = top; in Ratta's renderer also the list of layers drawn (6.1) | [verified]; [ratta-notelib] |
| `LAYERSWITCH` | absent | unknown | key [ratta-notelib]; not in the samples [verified] |
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
| `EXTERNALLINKINFO` | `0` | address of a block: `u32` length + text, records separated by `\|`, fields by `,`, each field base64 (several are numbers). Ratta turns each record into an external link in its PDF export (`fetchExternalLinkInfoForExport`) | [verified value 0]; layout [ratta-notelib]; field order [unknown] |
| `IDTABLE` | `0` | address, unknown content | [unknown] |
| `ORIENTATION` | `1000` | `1000` and `1180` portrait; `1090` and `1270` landscape; `2000`, `2090`, `2180`, `2270` a larger family (4.6) | `1000` [verified], `1090`/`1270` [reference; ratta-notelib], `1180` and the `2000` family [ratta-notelib] |
| `PAGETEXTBOX` | `0`, `1` | `1` on a page that has a text box, `0` otherwise (a count is possible, only 0 and 1 seen) | [verified: bike-digest page 3] |
| `DISABLE` | `none`, `256,796,271,88\|` | list of rects `left,top,width,height`, each followed by `\|`; `none` when empty. Each rect is a region whose pixels are device-drawn and have no ink strokes (text boxes; digests per others) | [verified: the rect equals the text box rect in the stroke data (7.4) and contains all pixels the text box added to MAINLAYER] |
| `FIVESTAR` | absent | present only on starred pages; value holds the star stroke coordinates and a trailing flag, exact layout not documented | [others] |

Keys of a page block written by newer firmware: `PAGETEXTBOX`, `DISABLE` [verified present in t1, t2, content page 1, all bike-digest pages; absent in bike and content page 2].

All address-valued page keys with value `0` mean "absent" [verified].

Ratta's renderer reads only `PAGESTYLE`, `LAYERSWITCH`, the five layer slots, `TOTALPATH`, `FIVESTAR`, `LAYERINFO`, `LAYERSEQ`, `PAGEID`, `EXTERNALLINKINFO` and `ORIENTATION` from a page block (`setInfoOfPageFeature`) [ratta-notelib].

### 4.4 LAYERINFO

The value is JSON with every `:` replaced by `#` [verified]. Replace `#` by `:` and parse as JSON (the reference also accepts base64-encoded JSON as a fallback [reference]). In the samples it is an array of 5 objects [verified], in t1:

| layerId | name | isBackgroundLayer | isCurrentLayer | isVisible | isDeleted |
|---|---|---|---|---|---|
| 3 | `Layer 3` | false | false | true | true |
| 2 | `Layer 2` | false | false | true | true |
| 1 | `Layer 1` | false | false | true | true |
| 0 | `Main Layer` | false | true | true | false |
| -1 | `Background Layer` | true | false | true | false |

Further fields per entry: `isAllowAdd`, `isAllowUp`, `isAllowDown` (booleans) [verified]. Mapping: `layerId` 0 = MAINLAYER, 1..3 = LAYER1..3, `isBackgroundLayer:true` = BGLAYER [reference; consistent]. A layer is drawn when `isVisible` is true [reference]. Ratta's renderer reads `LAYERINFO` but does not use its visibility when it draws; it draws what `LAYERSEQ` lists (6.1) [ratta-notelib]. Which rule the device screen follows is not known [unknown]; in the samples both give the same result [verified: all layers visible]. Layers 1-3 are marked deleted and have address `0` in the page block [verified]. Names are user-editable [others]. If MAINLAYER is missing from LAYERINFO it is visible [reference]. When `LAYERINFO` is `none`, visibility comes from `LAYERTYPE` (layers of type `MARK` visible) [reference; for `.mark` files].

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

Some firmware writes the background layer with `LAYERNAME:MAINLAYER`; a second MAINLAYER-named layer is then the background layer [reference]. A parser should identify layers by the page key that points to them (`BGLAYER:<addr>`), not by `LAYERNAME` [inferred]. Ratta does exactly this: it takes the slot names from `LAYERSEQ` (`MAINLAYER`, `LAYER1`-`LAYER3`, `BGLAYER`) and follows the page key of the same name; a listed slot with address `0` is skipped as transparent ("iLayerSite is 0,no this layer."), and an unknown slot name makes the whole page fail ("Unknown Layer Name...") [ratta-notelib]. A reader should skip an unknown name and report it [inferred].

### 4.6 Page size by device

| `APPLY_EQUIPMENT` | Device | Portrait width x height | Mark |
|---|---|---|---|
| `N6` | Supernote Nomad (A6 X2) | 1404 x 1872 | [verified] |
| `A6X` | Supernote A6X | 1404 x 1872 | [verified] |
| `N5` | Supernote Manta (A5 X2) | 1920 x 2560 | [reference; ratta-notelib] |
| any other | A5X, A5 and others | 1404 x 1872 | [reference; ratta-notelib: `A5X` and unknown codes use the 1404 family] |

Every bitmap in the samples decodes to exactly 1404 x 1872 = 2,628,288 pixels [verified]. The RLE stream itself does not state the size; the decoder needs it from the device code [verified]. For landscape pages (`ORIENTATION` 1090 or 1270) the bitmap is stored with width and height swapped, i.e. 1872 wide and 1404 high [reference; ratta-notelib].

Bitmap size by `ORIENTATION` in Ratta's renderer (width x height, `get_cimagesize`, `getCimageSizeByRotationType`) [ratta-notelib]:

| `ORIENTATION` | `N5` | all other devices |
|---|---|---|
| `1000`, `1180` | 1920 x 2560 | 1404 x 1872 |
| `1090`, `1270` | 2560 x 1920 | 1872 x 1404 |
| `2000`, `2180` | 3414 x 2560 | 2496 x 1872 |
| `2090`, `2270` | 2560 x 3414 | 1872 x 2496 |
| any other | logged as an error, then treated as `1000` | |

The last two digits read like a rotation in degrees (0, 90, 180, 270) [inferred]. The `2000` family has one side 4/3 as long (2496 = 1872 x 4/3); what it is for, maybe linked to the header key `HORIZONTAL_CHECK`, is not known [unknown]; none of the samples has it [verified]. The 300 dpi of the 1404 x 1872 devices follows from the stroke data (section 9.4) [verified: 0.0845 mm per pixel].

## 5. Bitmaps

### 5.1 Protocols

| Protocol name | Where | Encoding | Mark |
|---|---|---|---|
| `RATTA_RLE` | `LAYERPROTOCOL`, `LINKPROTOCAL`, `TITLEPROTOCOL` | run-length, 5.2 | [verified] |
| `SN_ASA_COMPRESS` | original A5 `PROTOCOL` | zlib stream of `u16` pixels, 1404 columns x 1888 rows stored rotated; rotate 90 degrees clockwise and drop the last 16 rows; codes `0x0000` black, `0x2104` dark gray, `0xE1E2` gray, `0xFFFF` background | [reference] |
| PNG | BGLAYER when `PAGESTYLE` starts with `user_` | a complete PNG file as block payload | [reference]; Ratta loads the payload as any image file Qt can read (`QImage::loadFromData`) and draws the note layers over it [ratta-notelib] |

Only RATTA_RLE occurs in the samples (14 layer bitmaps, 4 template bitmaps, 6 link bitmaps) [verified]. A binary block that starts with `89 50 4E 47` is a PNG [reference]; RLE blocks have no magic.

### 5.2 RATTA_RLE encoding

The payload is a sequence of 2-byte pairs `(code, len)` [verified]. Pixels are produced in row-major order, top row first, left to right [verified by pixel-exact match]. A layer with page size W x H produces exactly W*H pixels [verified on all 18 layer and template bitmaps].

Each pair is one run on its own [ratta-notelib: `uncompresseDataByRLE_15` and the old `uncompresseDataByRLE` decode every pair without looking at its neighbours; verified: on every current sample bitmap this gives exactly W*H pixels, identical to the reading below]. The run length is `(len & 0x7F) + 1`, multiplied by 128 when bit 7 of `len` is set:

| `len` | Run length |
|---|---|
| `0x00`-`0x7F` | `len + 1` pixels (1 to 128) of `code` [verified] |
| `0x80`-`0xFE` | `((len & 0x7F) + 1) * 128` pixels (128 to 16256) of `code` [verified] |
| `0xFF` | 16384 (`0x4000`) pixels of `code`, the same formula [verified] |

Ratta's encoder (`unitForRLE`) writes a run of n pixels as `floor(n / 16384)` pairs `(c, 0xFF)`, then, for the rest r, `(c, 0x80 + (r >> 7) - 1)` if r >= 128, then `(c, (r & 0x7F) - 1)` if `r & 0x7F` is not 0 [ratta-notelib]. So a long run is a "high" pair followed by a "low" pair of the same code, and the two add up to `(h + 1) * 128 + l + 1` with `h = len & 0x7F` of the first and `l` the `len` of the second [verified, 8,468 such combinations in the current layers of the samples]. A high pair followed by a different code is a run of exactly `(h + 1) * 128` [verified, 63 occurrences].

Earlier versions of this document read a high pair together with the next pair and called two other cases malformed: a high pair followed by a same-code high pair, and a high pair at the end of the stream. With independent pairs both are valid: two runs that add up, and a run of `(h + 1) * 128` [ratta-notelib]. Neither occurs in the samples [verified].

Ratta's decoder does not check the pixel total, has no bound on the output and ignores an odd trailing byte [ratta-notelib]. A reader should still stop at W*H pixels and report extra or missing pixels (section 10) [inferred].

Special blank length. The reference uses 1024 instead of 16384 for `0xFF` in one case: a BGLAYER of a `style_white` page whose bitmap block is exactly 5134 (`0x140E`) bytes long [reference]. Ratta's renderer has no such rule (no `0x140E` constant) [ratta-notelib], so it is a supernotelib quirk, not a format rule. The samples' `style_white` bitmap is 324 bytes and decodes correctly with 16384 [verified].

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

Two mappings from code to gray are known, and they differ on a few codes. supernotelib uses two tables, "X2" for signature versions 20230015 and higher and "X" for older files [reference]. Ratta's renderer uses one table for versions 20230015 and higher, and an old decoder that paints only a few codes below that version (5.2, section 1) [ratta-notelib]. The golden images and the test vectors in section 11 follow supernotelib's X2 table [verified pixel-exactly for every code that occurs]. Which mapping the device screen shows is not known [unknown]; a reader should offer both.

| Code | Name (supernotelib) | supernotelib X2 | supernotelib X (older) | Ratta, from 20230015 | Ratta, below 20230015 | Raw count in the current sample layers |
|---|---|---|---|---|---|---|
| `0x61` | black | 0 | 0 | 0 | 0 | many |
| `0x62` | transparent (background) | transparent | transparent | transparent | transparent | many |
| `0x63` | dark gray (X), compat dark gray (X2) | 0x30 | 0x9D | 0x30 | 0x30 | 24 (content p1) |
| `0x64` | gray (X), compat gray (X2) | 0x50 | 0xC9 | 0x50 | 0x50 | 12 (content p1) |
| `0x65` | white | 0xFE | 0xFE | 0xFE | 0xFE | 28 (bike-digest p3) |
| `0x66` | marker black | 0 | 0 | 1 | 1 | 2 (t1) |
| `0x67` | marker dark gray (X only) | 0x67 (literal) | 0x9D | 0x31 | 0x31 | 4 (bike-digest p3) |
| `0x68` | marker gray (X only) | 0x68 (literal) | 0xC9 | 0x51 | 0x51 | 2 (bike-digest p3) |
| `0x9D` | dark gray (X2) | 0x9D | n/a | 0x9D | not painted | 6 (bike-digest p3) |
| `0x9E` | marker dark gray (X2) | 0x9D | n/a | 0x9E | not painted | 5 (bike-digest p3) |
| `0xC9` | gray (X2) | 0xC9 | n/a | 0xC9 | not painted | 1 (bike-digest p3) |
| `0xCA` | marker gray (X2) | 0xC9 | n/a | 0xCA | not painted | 10 (bike-digest p3) |
| `0x00` | (level) | 0x00 | n/a | 0x61 | not painted | 0 |
| `0x01` | (level) | 0x01 | n/a | 0x66 | not painted | 3 (bike-digest p3) |
| `0x30` | (level) | 0x30 | n/a | 0x63 | not painted | 5 (bike-digest p3) |
| `0x31` | (level) | 0x31 | n/a | 0x67 | not painted | 5 (bike-digest p3) |
| `0x50` | (level) | 0x50 | n/a | 0x64 | not painted | 2 (bike-digest p3) |
| `0x51` | (level) | 0x51 | n/a | 0x68 | not painted | 0 |
| `0xFE` | (level) | 0xFE | n/a | 0x65 | not painted | 1 (bike-digest p3) |
| `0xFF` | (level) | 0xFF | n/a | 0x62 (opaque) | not painted | 4 (bike-digest p3) |
| any other | anti-aliasing level | the code value itself | n/a | the code value itself | not painted | many |

Counts are from the current MAINLAYER and BGLAYER bitmaps of all five sample files [verified]; codes not listed with a sample have count 0 everywhere.

Anti-aliasing [verified]: all X2 sample layers contain edge pixels with codes outside the named set. Pen strokes use mostly the 15 levels `0x0F, 0x1F, ..., 0xEF` (step 16); t1 also has scattered other values (`0x03`, `0x05`, `0x11`, ... `0xD1`). These are gray levels: the golden renderings show them with gray = code [verified].

Ratta's table is an involution [ratta-notelib: the 256-byte table `RLE_INDEX`, used by the decoder and by the encoder `unitForRLE`]. It is the identity except for eight swapped pairs: `0x61`/`0x00`, `0x62`/`0xFF`, `0x63`/`0x30`, `0x64`/`0x50`, `0x65`/`0xFE`, `0x66`/`0x01`, `0x67`/`0x31`, `0x68`/`0x51`. The encoder writes gray g as code `RLE_INDEX[g]`, the decoder turns code c into gray `RLE_INDEX[c]`. So the named codes are also the stored form of the grays they displace: gray 0x30 is stored as code `0x63`, and gray 0x62 as code `0xFF`, so no gray level is lost to the transparent code. The codes `0x9D`, `0x9E`, `0xC9`, `0xCA` keep their value; Ratta does not merge `0x9E` into `0x9D` or `0xCA` into `0xC9` as supernotelib does.

The samples support the involution [verified]. In content page 1 the codes `0x63` and `0x64` occur only inside the device-drawn link icons, among levels `0x10, 0x20, ..., 0xE0`; raw codes `0x30` and `0x50` do not occur on that page, so `0x63`/`0x64` fill exactly the two missing steps 0x30 and 0x50. The glyph edges of the device-drawn text box on bike-digest page 3 contain raw codes `0x01`, `0x30`, `0x31`, `0x50`, `0xFE`, `0xFF`, `0x65`, `0x67`, `0x68` among plain levels, which is what an 8-bit gray image looks like after the involution. An earlier version of this document read `0x63`/`0x64` as plain levels 0x63/0x64; that inference was wrong. Both supernotelib and Ratta map them to 0x30 and 0x50.

Below 20230015 [ratta-notelib]: Ratta's old decoder paints only `0x61` (0x00) and `0x63`-`0x68` (0x30, 0x50, 0xFE, 0x01, 0x31, 0x51) and skips every other code, anti-aliasing levels included; the layer streams are merged before decoding (`combineRLEData`). supernotelib's X table maps `0x63`/`0x67` to 0x9D and `0x64`/`0x68` to 0xC9 instead. No sample is older than 20230015 [verified], and the device screen is not known for these files [unknown], so both mappings are listed.

Output palettes in Ratta's renderer [ratta-notelib, `fetchNoteRLEData_15`]: with a built-in template the composite becomes an 8-bit gray image directly (gray = `RLE_INDEX[code]`, unpainted pixels 255), so black is 0. With a `user_` template the composite goes through `NOTE_COLOR_CONVERT_INDEX`, an ARGB table that is opaque gray i for index i except index 0 (gray 1) and 0xFF (fully transparent), and is drawn over the template image; there black is 1, as in the device export (6.2). The `.mark` palette is different (6.4).

Pixels where Ratta's renderer differs from notedown's golden images and test vectors today (current layers, per the table above) [verified by code counts]:

- t1 page 1: 2 pixels (`0x66`: 0 versus 1). This is the only golden image affected.
- bike and content goldens: 0 pixels (`0x63`/`0x64` map the same in both).
- bike-digest page 3 (test vector 11.3a, no golden image): 41 pixels, by code `0x01` 3, `0x30` 5, `0x31` 5, `0x50` 2, `0xFE` 1, `0xFF` 4, `0x67` 4, `0x68` 2, `0x9E` 5, `0xCA` 10. Some steps are large: `0x30` becomes 0x63 instead of 0x30, `0x67` becomes 0x31 instead of 0x67, `0xFF` becomes 0x62 instead of 0xFF.

### 5.4 Template layer (BGLAYER)

The BGLAYER bitmap is the page template [verified]. Its block is shared: every page with the same `PAGESTYLE` points to the same bitmap block, and the footer lists it as `STYLE_<name>` [verified: bike pages 1-3 all use 439]. The ruled template `style_8mm_ruled_line` is 462 bytes and byte-identical in all four files [verified]. Templates in the samples contain only black (`0x61`) and transparent pixels [verified]. Footers may list templates no page uses (bike and content list `STYLE_style_white`) [verified]. Built-in templates are RLE layers inside the file; Ratta's renderer draws them from the file and does not use its own template pictures for these files (those serve only the original A5 path) [ratta-notelib]. When `PAGESTYLE` starts with `user_`, the BGLAYER block is a complete image file instead (5.1) [reference; ratta-notelib].

## 6. Rendering a page

### 6.1 Compositing (matches the golden renderings)

1. Decode each layer with an address other than 0 to W*H codes [verified].
2. Determine visible layers from `LAYERINFO` (4.4) [reference; all layers visible in the samples]. Ratta's renderer skips this step and draws every layer listed in `LAYERSEQ` [ratta-notelib].
3. Start with a canvas of gray 255 (white) [verified].
4. Go through `LAYERSEQ` from last to first (bottom to top). For each visible layer, every pixel whose code is not `0x62` replaces the canvas pixel with the code's gray value from 5.3. Transparent pixels leave the canvas unchanged [verified].
5. The result is an 8-bit gray image. The goldens are RGBA PNGs with R = G = B = gray and A = 255 [verified].

Without template ("nobg" goldens): skip BGLAYER, and output A = 0 with R = G = B = 255 where no layer covered the pixel [verified].

With these rules all 14 golden PNGs (7 pages, with and without template) are reproduced with 0 differing pixels [verified]. The order matters where ink overlaps template lines: in t1, 169 MAINLAYER pixels lie on template lines and the MAINLAYER value wins [verified].

Replacement compositing means anti-aliased edge pixels show their own level even over a black template line. A reader that wants alpha blending must do it itself (6.2).

Ratta's renderer composites the same way [ratta-notelib, `A6X_saveNotePagePic`, `combineBitmapData`]: the first `LAYERSEQ` entry is on top, each next layer is put below it, and a pixel of the upper layer wins unless it is transparent; anti-aliasing pixels replace what is below. Special `LAYERSEQ` values: empty means `LAYER3,LAYER2,LAYER1,MAINLAYER,BGLAYER`, `none` means `BGLAYER` only, `nolayer` gives a blank page. Listed layers with address 0 are skipped (4.5). Its colors follow its own table and palettes (5.3), so its output differs from the goldens on the pixels listed there. No mode that leaves out the template was found [ratta-notelib].

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
- Cover image: a block of `u32` length + the image file. `COVER_1` and `COVER_2` hold a PNG, `COVER_3` a JPEG; `COVER_0` means no cover; other numbers are unknown ("unkown cover type.") [ratta-notelib, `fetchFileCover`, which the renderer never calls; no sample has a cover, verified]. All surveyed community parsers expose it only as raw bytes [others]. (Answered open question: the encoding was unknown before.)
- No thumbnails are stored in the samples (`THUMBNAILTYPE:0`) [verified].

### 6.4 `.mark` files [ratta-notelib; no `.mark` sample, verified]

A `.mark` file holds handwriting drawn over a PDF. It uses the same container, footer, page, layer and RLE rules as a `.note`, with file type `mark` (section 1).

- Only the MAINLAYER of a mark page is rendered (`A6X_saveMarkPagePic`).
- Page size comes from the mark page's `ORIENTATION` and the header's `APPLY_EQUIPMENT` (unknown codes as `N6`), as in 4.6.
- From `markSN_FILE_VER_20230015` on: the "15" decoder and table (5.2, 5.3), then the palette `MARK_COLOR_CONVERT_INDEX` into an ARGB image: gray 0 and 1 become opaque gray 1; gray 2 to 0xFD become black with alpha `255 - gray`; 0xFE stays opaque 0xFE; 0xFF is transparent. So a gray level acts as ink coverage over the PDF. Older marks use the old decoder and are then rotated.
- Page mapping (`A6X_producePDFbyMarkAndPdf`): for each PDF page n the renderer draws mark page n, and skips mark pages without ink (`markPageIsEmpty`). That the footer then has `PAGE<n>` keys with n the PDF page number and no key for pages without ink is [inferred]; a reader must expect gaps in the page numbers.
- Rotation: the mark image is rotated by minus the PDF page rotation; for a PDF page wider than high by -90 minus the rotation; for a mark `ORIENTATION` of 1090 or 1270 another 90 degrees are added. The image is placed over the whole PDF page; how the PDF library scales it is not known [unknown].
- Link to the PDF: by file name only, not by any id in the mark (`pdfWithMarkToPdf` in Ratta's Java server). For `x.pdf` the mark is `x.pdf.mark` in the same folder, else `x_PDF.mark`.

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

Ratta's renderer [ratta-notelib, `A6XSnLinkFeatureModule`] reads `LINKTYPE`, `LINKINOUT`, `LINKBITMAP`, `LINKSTYLE` (default `1000000`), `LINKTIMESTAMP`, `LINKRECT`, `LINKRECTORI`, `LINKPROTOCAL` (default `RATTA_RLE`), `LINKFILE`, `LINKFILEID`, `PAGEID` and `OBJPAGE`; missing string keys default to `none`. It does not know the text link keys `FONTSIZE`, `FONTPATH`, `FULLTEXT`, `SHOWTEXT`, `ANTI`, `BOLD`, `ITALIC`. In its PDF export, `LINKO_` links become internal PDF links over `LINKRECT` (split on `,` into four integers, `LINKFILE` base64-decoded), and `EXTERNALLINKINFO` records (4.3) become external links; a link without a file id is skipped ("fileid not set, do not process link"). Which `LINKTYPE` values end up as internal or external links was not traced [unknown].

The device draws a link icon at the right end of the link rect into MAINLAYER [verified: content page 1, the icons lie at the right ends of the six rects]. A link also leaves records in the page's stroke data (9.6).

### 7.4 Text boxes and note digests [verified on bike-digest unless marked]

A text box is typed text placed on a page. A note digest is created by circling handwriting with the lasso tool and choosing "Recognize as Text" [Ratta support documentation]; the device then replaces the handwriting by a text box holding the recognized text and registers a digest in the Digest app. bike-digest page 3 holds exactly one such text box. The pieces in the file:

| Piece | Where | Content |
|---|---|---|
| page flag | page key `PAGETEXTBOX:1` | the page has a text box |
| region | page key `DISABLE:256,796,271,88\|` | the text box rect (`left,top,width,height`) |
| image | MAINLAYER bitmap | the typeset text, drawn by the device: 4,424 pixels changed against bike, all inside the rect (x 259 to 523, y 820 to 872), black plus anti-aliasing levels. The text is shown between square brackets and underlined |
| source handwriting | 7 ink records in TOTALPATH (records 52 to 58, ids 53 to 59) | ordinary needle-pen strokes with status `-5` (part G). They are not drawn: none of their pixels is in the bitmap |
| lasso | 1 record (record 59, id 60): tool 4, class -5, operation 604 | the selection loop |
| text box record | 1 record (record 60, id 61), see below | rect, text, font, style, digest reference, embedded copy of the source strokes |
| group record | 1 record (record 61, id 62), see below | ties the source strokes and the text box together |

No footer key, no new block kind and no other key refers to the text box [verified: every block of bike-digest is reachable from the footer and accounted for; the only new keys are `PAGETEXTBOX` and `DISABLE`].

**Text box record** (stroke record layout of 9.3):
- head: tool 0, color 0, thickness 100, offset 12 = 0, class 0, kind name `0000` (the same kind name as a link box), document kind `name is not set`; digitizer ranges 15819/11864; the bbox fields read `256, 796, 0, 0, 754, 1111` (meaning of the last two [unknown]);
- 5 points, all pressure 0; one outline ring = the rect as a closed 5-point polygon (256,883), (526,883), (526,796), (256,796), (256,883);
- status 0, operation `52` (link boxes use 500), referenced ids: none;
- first string: base64 items separated and terminated by commas, 19 items, the same layout as a link box (9.6). Decoded: `0`, `0`, `0`, creation timestamp `YYYYMMDDhhmmss` + microseconds (local time), rect, original rect, `none`, `none`, `none`, `0`, font size `48.000000`, font path `/system/fonts/DroidSansFallbackFull.ttf`, the text (UTF-8), a style list of 18 comma-separated numbers (`4,0,0,0.000000,0.000000,1.000000,0,1,0,0,0,0,0,3,0,255,255,0`, meaning [unknown]), `63` [unknown], `1`, `1`, `0` (anti-alias, bold, italic by analogy with links [inferred]), and a JSON object `{"digest_source":"1","digest_id":"<32 hex digits>","digest_time":"<ms since epoch>"}`;
- third string of part P (empty on ink): an extension payload of 14667 bytes: ASCII `TEXTBOX_TRAILS`, `u32` count (7), `u32` 1000 [unknown], `u32` 4 [unknown], then count x (`u32` length + stroke record). The embedded records are byte-identical to the 7 source stroke records except the status field, which is 0 in the copy and -5 on the page [verified].
- The record class of a text box is 0 like link boxes; tell them apart by the operation (52 versus 500), the document kind and the digest JSON [verified on the samples, rule inferred].

**Group record** (record 61): tool 4, class 5000, kind name `others`, document kind `name is not set`, no points, no rings, operation 52, referenced ids = the 7 source stroke ids and the text box id. Its color and thickness fields (offsets 4 and 8) and maxB (offset 132) hold large values that are not colors or ranges (`644497065`, `1527775965`) [unknown]; the part P `u32` that is 3 on other records is 1 [verified]. A parser must not draw it and must not use its ranges.

**Digest link** [verified values, meaning from others]: `digest_time` 1791270408460 ms = 2026-10-06 07:06:48.460 UTC, the same instant as the item timestamp `20261006090648924962` in local time (UTC+2). `digest_id` has the form of the 32-hex-digit `unique_identifier` that the Supernote cloud sync protocol uses for digest items [others]. A note digest item in that protocol carries `note_fileId`, `note_pageId` and `note_page` of its source page, `content` (the recognized text), optional `commentStr` (typed annotation) and an optional handwritten annotation as a separate `.mark` file named `<md5>.mark` [others: ultrabridge protocol notes]. So the link from the digest back to the note is FILE_ID + PAGEID + page number, and the link from the note to the digest is `digest_id` [inferred].

**Not in the `.note` file**: the digest record itself (category, typed comment) and the handwritten annotation written in the Digest app [verified by exhaustion: no block, key or record in bike-digest holds handwriting other than the page's own strokes and the 7 embedded source strokes]. Per others, the device keeps digests in the Digest app's own storage and syncs them as structured records ("summary" items) with the handwriting as `.mark` files (stored by the cloud under a `digest` folder) [others]. The device folder for these files was not found in any public source [unknown].

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

**G. Status** `i32` [verified]: `0` = the stroke is visible; `-99` = the stroke was erased; `-5` = the stroke was converted into a text box and is hidden (bike-digest page 3, 7 records, none drawn; see 7.4). Verified: t2 record 78 (ink, `-99`) has only 9% of its points on ink, all live ink records have 100%; content page 1 has 8 records with `-99` (0% to low coverage) and 5 eraser records that together list exactly these 8 ids (one eraser lists 4). Others report further codes (-16 lasso delete, -4 partially erased with fragments in later records, -3 moved by lasso, -2 whole page cleared) [others].

**H. Operation** `i32` [verified]: `0` on ink (`99` on 20 of content page 2's 48 ink records), `601` eraser, `604` lasso path, `500` link box, `52` text box and its group record (7.4). Others report 602, 603 (other eraser modes), 400 (heading box), 14/4/2 (operation applied by a lasso: delete, move, copy) [others].

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
- `u32` length + bytes: empty (length 0) on ink, lasso, eraser and link records; on a text box record an extension payload starting with ASCII `TEXTBOX_TRAILS` (7.4) [verified].
- `u32` 3 [verified on all records except the text box group record, which has 1].
- `u32` n, then n x 8 bytes (`f32` pairs, "marker fill direction" [others]) [verified: n is 0 in bike and older content records, 2 with zero values in t1, t2 and newer content records].
- Only in records written by 20260016 firmware: 4 more zero bytes [verified: t1, t2, bike-digest page 3; absent in bike-digest pages 1 and 2, see section 1].

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
- kind name `0000` (link box or text box; the rectangle is not drawn by the device, the text box text is in the bitmap only);
- operation 52 group records and any record with tool 4 (7.4);
- status -5 (handwriting replaced by a text box) is already covered by "status not 0". To show the original handwriting of a text box, use the embedded copies in the text box record [inferred].
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
- Device-drawn content without strokes: link icons [verified], text box text (`DISABLE` regions) [verified on bike-digest], digest excerpts [others], and anything pasted as an image [inferred]. These exist only in the bitmap.
- Erased strokes and eraser/lasso paths stay in TOTALPATH (append-only per page until compaction) [verified]. The firmware removes fully erased strokes at some point [others]; bike (compacted) has no erased records [verified].

## 10. Robustness rules for a parser

Out-of-bounds risks and invariants [inferred unless marked]:

1. File shorter than 32 bytes or footer address outside `[24, len-8)`: reject or fall back to a sequential walk (2.) to find the last valid footer. A missing `tail` before the last 4 bytes alone is a warning: Ratta's renderer does not check it [ratta-notelib].
2. Every address: check `addr + 4 <= len` and `addr + 4 + N <= len` before reading a block. Address 0 means absent; do not read offset 0.
3. Metadata: decode as UTF-8 with replacement on error; never trust a value to be numeric. Parse integers with overflow checks; addresses must fit `u32`.
4. Cycles cannot occur in a well-formed file (addresses point to blocks, never back to the footer), but a corrupt file can point a page key at the footer itself; detect re-visits.
5. RLE: stop when W*H pixels are produced and report extra input; report short output instead of panicking. A run must not write past W*H. Decode every pair on its own (5.2): a high pair after a high pair and a high pair at the end are valid runs for Ratta [ratta-notelib]. An odd trailing byte is ignored by Ratta [ratta-notelib]; report it. Ratta checks neither the pixel total nor the output bounds, so these checks are the reader's own.
6. Page size: determine from `APPLY_EQUIPMENT` and `ORIENTATION` before decoding (4.6); an unknown device code should fall back to the 1404 family (as Ratta does, treating it as `N6`) and be reported; an unknown orientation should fall back to `1000` and be reported [ratta-notelib for both fallbacks].
7. TOTALPATH: check `R` against the block length (each record needs at least 4 bytes); check each record length against the remaining bytes; inside a record check every count against the remaining record bytes (`n * size`) before allocating. Point counts in parts B to E must be equal [verified invariant]; ring count in N must equal the ring count in M [verified invariant].
8. Digitizer range fields of 0 must not be used for scaling; others report one non-stroke record with a range of 120 [others]. Skip points of records whose range is implausible.
9. RECOGNFILE: treat as an untrusted ZIP (size limits, path traversal in entry names).
10. RECOGNTEXT: base64 then JSON; both can fail.
11. Unknown signature: parse with these rules, mark as unrecognized; versions are ordered by string compare, not by an allow-list (section 1) [ratta-notelib]. Unknown `LAYERPROTOCOL`: expose the raw block.
12. Unknown keys anywhere: keep them as raw strings.
13. Metadata items that do not split into exactly key and value on `:`, and text after a NUL byte, are dropped by Ratta (3) [ratta-notelib]; keep them, but flag them.
14. A `LAYERSEQ` entry that is not one of the five slot names: Ratta fails the page [ratta-notelib]; skip the entry and report it [inferred].

Plausible invariants worth checking [verified on samples]: `tail` before each footer address; the footer address points to a block that ends exactly 8 bytes before end of file; footer `FILE_FEATURE` points to a block whose first key is `MODULE_LABEL` or `FILE_TYPE`; `LAYERSEQ` lists only layers with a non-zero address; each layer bitmap decodes to exactly W*H; record page number (9.3 A offset 28) equals the page.

## 11. Test vectors

All values computed by an independent decoder written from this document, and cross-checked against supernotelib 0.7.3 (layer decoding with the X2 table, raw TOTALPATH and RECOGNFILE blocks): all hashes and counts agree [verified]. The "gray" hashes follow supernotelib's X2 table, not Ratta's table; with Ratta's table the t1 MAINLAYER and composite hashes and the bike-digest page 3 hash would change (2 and 41 pixels, 5.3), all other gray hashes stay the same [verified by code counts].

Definitions:
- "codes" = the decoded RLE output, one byte per pixel, the raw code values, row-major.
- "gray" = codes mapped with supernotelib's full X2 table of 5.3 (`0x61` -> 0, `0x63` -> 0x30, `0x64` -> 0x50, `0x65` -> 0xFE, `0x66` -> 0, `0x9E` -> 0x9D, `0xCA` -> 0xC9, other codes unchanged), transparent -> 255.
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

### 11.3a bike-digest (local only, 1135077 bytes)

| Item | Value |
|---|---|
| footer / header | 1134930 / 24 (header block byte-identical to bike) |
| pages 1, 2 | page blocks 544370, 958898; bitmaps and TOTALPATH blocks byte-identical to bike |
| page 3 block | 1133641, `PAGETEXTBOX:1`, `DISABLE:256,796,271,88\|` |
| page 3 MAINLAYER bitmap | 960176, 28802 bytes, opaque 22775, sha256(gray) `7229c7bfcd2818f1070c1c33530dc8e3d0e1958afcf8a5fd72dd1fdd4defba12` (matches supernotelib) |
| page 3 TOTALPATH | 989245, 144392 bytes, 64 records: 52 old ink (ids 1 to 52), 7 ink with status -5, 1 lasso, 1 text box, 1 group record, 1 erased ink tap and 1 eraser; sha256 `d9dbdcb4ffc594f2e1bd31ea004c75b713dac7596bbe55f248a84e495bc30ef4` |
| text box record | record 60, 15609 bytes, rect 256,796,271,88, 7 embedded strokes |

### 11.4 content (local only, 1681602 bytes)

| Page | Page block | MAINLAYER bitmap (bytes, opaque, sha256 gray) | TOTALPATH (address, records, sha256) |
|---|---|---|---|
| 1 | 1679987 | 1431290 (41256, 40903, `2f951b16a780880d67b118b4deea9d91dce9a3b6e9246e270fff77856a08a2d1`) | 1474300, 76: 53 live ink, 8 erased ink, 5 eraser, 4 lasso, 6 link box; `77db68565a3e55af021717bcd57af973dd61c04873d8e3727232daebe4490689` |
| 2 | 283760 | 148564 (29070, 21879, `f1dfb516dced09dba4d53f0847bd600c693846f97d365da0ab838e02cf4579bf`) | 177902, 48, `becab8806026defc77554bc637ca687b99d3b2b9cc6a09a46fb8c1745fa06121` |

Links: 6 blocks at 452, 843, 1252, 1638, 500079, 1180240, all `LINKTYPE:1`, `LINKINOUT:0`, page 1; bitmaps at 440, 831, 1242, 1626, 496157, 1174576. The block at 1252 targets bike (7.3).

### 11.5 Golden images

Rendering per 6.1 reproduces all of `tests/fixtures/golden/test{1,2}-p1-{bg,nobg}.png` and `tests/fixtures/local/golden/{bike-p1..p3,content-p1..p2}-{bg,nobg}.png` with 0 differing pixels [verified]. The goldens follow supernotelib's X2 table. Rendered with Ratta's table (5.3) they would differ only in t1 page 1, on the 2 pixels of code `0x66` (gray 1 instead of 0), in both the `bg` and `nobg` variant [verified by code counts].

## 12. Unknowns and open items

Open questions answered by Ratta's renderer [ratta-notelib]: cover encoding (answered: PNG or JPEG, 6.3); the 1024/5134 `style_white` rule (answered: a supernotelib quirk, 5.2); `0x63`/`0x64` (answered: gray 0x30/0x50 by the involution, the earlier anti-aliasing reading was wrong, 5.3); RLE edge cases of a high pair (answered: valid, pairs are independent, 5.2); `tail` check (answered: not checked); page size for landscape (answered and extended, 4.6). Partly answered: `HIGHLIGHTINFO` (block shape known, content not), `EXTERNALLINKINFO` (record layout and use as external PDF links known, field order not), `.mark` rendering (6.4, still no sample).

Still unknown after Ratta's renderer:

- The device screen's color mapping for files below 20230015 and for the swapped codes in X2 files (5.3); this needs a rendering from the device.
- Whether the device honors `LAYERINFO` visibility (4.4); Ratta's renderer does not.
- Meaning of the `2000` orientation family (4.6), maybe linked to `HORIZONTAL_CHECK`.
- `LAYERSWITCH`, `ORIGINAL_STYLE`, header `COVER` value, `EXTERNALLINKINFO` field order, `HIGHLIGHTINFO` content.
- Effect of the background flag for signatures below `SN_FILE_VER_20210009` (section 1).
- Which `LINKTYPE` values become internal or external PDF links (7.3); shape of `LINKI_` footer keys.
- How the PDF library scales the `.mark` image on the PDF page (6.4).
- Everything Ratta's renderer does not use: TOTALPATH, RECOGNTEXT and RECOGNFILE, titles, keywords, text boxes (`PAGETEXTBOX`, `DISABLE`), digests, `FIVESTAR`, anything only in 20260016.

Raw-only fields (expose as raw data): text box style list, item 15 (`63`) and the bbox values of the text box record, the `1000`/`4` values in `TEXTBOX_TRAILS`, color/thickness/range fields of the group record, `digest_source`; header `MODULE_LABEL`, `FILE_PARSE_TYPE`, `RATTA_ETMD`, `APP_VERSION` (`APP_VERTION`), `ORIGINAL_STYLE`, `COVER`, `PDFSTYLE`, `PDFSTYLEMD5`, `STYLEUSAGETYPE`, `HORIZONTAL_CHECK`, `IS_OLD_APPLY_EQUIPMENT`; page `LAYERSWITCH`, `THUMBNAILTYPE`, `RECOGNTYPE`, `RECOGNLANGUAGE`, `EXTERNALLINKINFO` (record fields), `IDTABLE` (the last two address-shaped, 0 in samples); layer `LAYERPATH`, `LAYERVECTORGRAPH`, `LAYERRECOGN` (address-shaped, 0 in samples); link `LINKSTYLE`, `FONTSIZE`, `FONTPATH`, `FULLTEXT`, `SHOWTEXT`, `ANTI`, `BOLD`, `ITALIC`; stroke head offsets 12, 16, 20, 24, 32, 36, 124, 188; tilt semantics; render flag; trailer values; `page.bdom`, `style.css`, the `ink.bink` element table.

Not present in the samples: titles, keywords, stars, covers, thumbnails, custom (PNG) or PDF templates, landscape pages, layers 1-3, `SN_ASA_COMPRESS`, X-table files (versions below 20230015), `.mark` files, `LINKI_` links, `COVER_3`, the `1180` and `2000` orientations, RECOGNTEXT, N5 (1920 x 2560) files, other pens than needle point, other colors than black, lasso move/delete, partial erase, typed text boxes (not created from handwriting), document digests, the digest record and its handwritten `.mark` annotation (stored outside the `.note`, 7.4). Each of these needs a sample before its rules can be verified.

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
| Text box | page, rect (from the record ring and `DISABLE`), text, font size, font path, style list raw, flags, creation timestamp, ids of the hidden source strokes and their embedded copies, digest reference (`digest_id`, `digest_time`, `digest_source`) | 4.3, 7.4, 9.3 |
| Digest reference | digest id, time, source; the note side of a digest (file id, page id, page number); the digest content and annotation are external | 7.4 |
| Link | footer key with decoded page and position, all link keys, decoded target path or URL, target file id, target page id, kind, direction, rect, timestamp, bitmap, link-box stroke record and linked stroke ids | 4.1, 7.3, 9.6 |
| Title | footer key with page and position, rect, original rect, level, sequence number, style (decoded background and text gray), protocol, bitmap | 7.1 |
| Keyword | footer key with page and position, page, rect, original rect, sequence number, text (from `KEYWORDSITE` and inline), length | 7.2 |
| Cover | footer key (`COVER_0` to `COVER_3`), address, image kind (PNG or JPEG), raw bytes | 4.1, 6.3 |
| Recognition | RECOGNTEXT decoded JSON (elements, labels, word boxes), RECOGNFILE archive entries, `ink.bink` strokes with timestamps and mm coordinates, raw `page.bdom`/`style.css` | 8 |

## Appendix B. Coverage compared with other parsers

Projects surveyed (feature inventory only): supernotelib (reference, Python), supernote-typescript (TypeScript, also used by supernote-obsidian-plugin), calamus (Rust, format notes plus bitmap and stroke rendering), sntool and snorg (Go; snorg uses sntool), sn2md (Python, uses supernotelib; its templates show the fields users want), supernote-obsidian-plugin, allenporter/supernote (Python server, own copy of a supernotelib-style parser), ultrabridge (Go server; reads keywords, titles and RECOGNTEXT, writes RECOGNTEXT).

"Required" = at least one other parser extracts it; notedown's parser must match it. "Beyond baseline" = found only in this work.

| Data kind | supernotelib | supernote-typescript | calamus | sntool/snorg | sn2md | allenporter | ultrabridge | This doc | Status |
|---|---|---|---|---|---|---|---|---|---|
| envelope, signature, footer/header/page/layer metadata | yes | yes | yes | yes | via lib | yes | partial | 1-4 verified | required |
| append-only history, stale block handling | current only | current only | current only | current only | via lib | current only | - | 2 verified, history exposed | required (current state); history beyond baseline |
| RATTA_RLE, X and X2 tables, anti-aliasing levels | yes | yes | yes | yes | via lib | yes | - | 5.2, 5.3 verified; Ratta's tables 5.3 | required |
| SN_ASA_COMPRESS (original A5) | yes | - | yes | yes | via lib | yes | - | 5.1 reference | required, not in samples |
| PNG custom templates | yes | yes | yes | yes | via lib | yes | - | 5.1 reference | required, not in samples |
| layer visibility from LAYERINFO, LAYERSEQ order | yes | yes | yes | yes | via lib | yes | - | 4.4, 6.1 verified; Ratta uses LAYERSEQ only | required |
| landscape orientation | yes | yes | - | yes | via lib | yes | - | 4.6 reference, ratta-notelib (with `1180` and the `2000` family) | required, not in samples |
| page rendering (PNG) | yes | yes | yes | yes | via lib | yes | - | 6 verified pixel-exact | required |
| SVG by tracing bitmaps | yes | yes | yes | yes | via lib | yes | - | (renderer concern) | required (output feature) |
| keywords (text, page, rect, bitmap) | yes | yes | - | yes | yes | yes | yes | 7.2 reference | required, not in samples |
| titles (rect, bitmap, level, seq no) | yes (rect, bitmap) | yes | - | yes | yes (level) | yes | yes | 7.1 reference | required, not in samples |
| title style (`TITLESTYLE`) | - | yes | - | - | - | - | - | 7.1 others | required, not in samples |
| links (type, direction, path/URL, file id, page id, rect, timestamp) | yes | yes | - | yes | yes | yes | - | 7.3 verified | required |
| link `OBJPAGE` target page | - | yes | - | yes | - | - | - | 7.3 value verified | required |
| link bitmap | stored | yes | - | yes | - | yes | - | 6.3 verified (size = rect) | required |
| stars (`FIVESTAR`, star stroke kind) | - | yes | - | yes | - | - | - | 4.3, 9.3 others | required as presence flag plus raw value; value layout undocumented everywhere |
| cover image | yes | yes | - | yes | - | yes | - | 4.1, 6.3 ratta-notelib (PNG/JPEG) | required as raw bytes plus image kind; not in samples |
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
| text boxes (`DISABLE`, `PAGETEXTBOX`, text box records) | - | yes | - | - | - | - | - | 4.3, 7.4 verified | required |
| text box text, font and digest reference from the stroke record | - | - | - | - | - | - | - | 7.4 verified | beyond baseline |
| hidden source handwriting of a text box (status -5, `TEXTBOX_TRAILS`) | - | - | - | - | - | - | - | 7.4 verified | beyond baseline |
| digest records and their `.mark` handwriting (cloud sync) | - | - | - | - | - | yes (sync events) | yes (stores and serves them) | 7.4 others | outside the `.note`; not required for a `.note` parser |
| heading rects, ruler lines, partial-erase fragments | - | yes | - | - | - | - | - | 9.5 others | required, not in samples |
| vector freshness versus bitmap | - | heuristic | - | - | - | - | - | 9.7 verified pattern | beyond baseline |
| device export compositing (multiply, black = 1) | - | - | - | - | - | - | - | 6.2 verified | beyond baseline |
| Ratta's color table (involution, swapped codes `0x00`/`0x01`/`0x30`/`0x31`/`0x50`/`0x51`/`0xFE`/`0xFF`) | - | - | - | - | - | - | - | 5.3 ratta-notelib, supported by sample codes | beyond baseline (replaces the wrong `0x63`/`0x64` anti-aliasing inference) |
| per-save history of headers and pages | - | - | - | - | - | - | - | 2 verified | beyond baseline |
| `.mark` files | yes | partial | doc only | - | - | - | yes (PDF side) | 6.4 ratta-notelib | required, not in samples |
| `HIGHLIGHTINFO` | - | - | doc only | - | - | - | - | 4.2 value verified | raw only |
| writing RECOGNTEXT into files | - | - | - | - | - | - | yes | out of scope (read-only) | not required for a reader |
