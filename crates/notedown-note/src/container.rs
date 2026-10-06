//! File envelope, block access and the sequential walk (docs/note-format.md 1, 2, 10).

use crate::error::{NoteError, corrupt};

const VER_PREFIX: &[u8] = b"SN_FILE_VER_";
const ASA_PREFIX: &[u8] = b"SN_FILE_ASA_";
const SIGNATURE_LEN: usize = 20;
const TAIL: &[u8] = b"tail";

/// The fixed prefix of the file (docs/note-format.md 1).
#[derive(Debug)]
pub(crate) struct Envelope {
    pub(crate) file_type: String,
    pub(crate) signature: String,
    pub(crate) version: Option<u32>,
    /// Offset of the first block: 24, or 20 for the original A5 layout.
    pub(crate) first_block: u64,
}

/// `prefix` followed by 8 decimal digits.
fn signature_digits(bytes: &[u8], prefix: &[u8]) -> Option<u32> {
    let digits = bytes.strip_prefix(prefix)?;
    if bytes.len() != SIGNATURE_LEN || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

/// Reads the file type and signature. Anything else is `NotANote`
/// (docs/note-format.md 1).
pub(crate) fn envelope(data: &[u8]) -> Result<Envelope, NoteError> {
    // Original A5: the signature replaces the file type field.
    if let Some(sig) = data.get(..SIGNATURE_LEN)
        && let Some(version) = signature_digits(sig, ASA_PREFIX)
    {
        return Ok(Envelope {
            file_type: String::new(),
            signature: String::from_utf8_lossy(sig).into_owned(),
            version: Some(version),
            first_block: SIGNATURE_LEN as u64,
        });
    }
    let file_type = data.get(..4).ok_or(NoteError::NotANote)?;
    let sig = data.get(4..4 + SIGNATURE_LEN).ok_or(NoteError::NotANote)?;
    let version = signature_digits(sig, VER_PREFIX).ok_or(NoteError::NotANote)?;
    if !file_type.iter().all(u8::is_ascii_graphic) {
        return Err(NoteError::NotANote);
    }
    Ok(Envelope {
        file_type: String::from_utf8_lossy(file_type).into_owned(),
        signature: String::from_utf8_lossy(sig).into_owned(),
        version: Some(version),
        first_block: (4 + SIGNATURE_LEN) as u64,
    })
}

fn read_u32(data: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    let bytes: [u8; 4] = data.get(at..end)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

/// The payload of the block at `address` (docs/note-format.md 2, 10.2).
/// The error text describes the problem; callers add the key name.
pub(crate) fn payload(data: &[u8], first_block: u64, address: u64) -> Result<&[u8], String> {
    if address == 0 {
        return Err("address 0 means absent".into());
    }
    if address < first_block {
        return Err(format!("address {address} lies inside the file prefix"));
    }
    let start = usize::try_from(address).map_err(|_| format!("address {address} is too large"))?;
    let len = read_u32(data, start).ok_or_else(|| {
        format!(
            "block at {address}: length field past end of file ({} bytes)",
            data.len()
        )
    })?;
    let len =
        usize::try_from(len).map_err(|_| format!("block at {address}: length {len} too large"))?;
    let body = start.checked_add(4);
    let end = body.and_then(|b| b.checked_add(len));
    match (body, end) {
        (Some(body), Some(end)) => data.get(body..end).ok_or_else(|| {
            format!(
                "block at {address}: length {len} runs past end of file ({} bytes)",
                data.len()
            )
        }),
        _ => Err(format!("block at {address}: length {len} overflows")),
    }
}

/// Finds the current footer from the trailing `tail` marker and checks that the
/// footer block ends exactly at the marker (docs/note-format.md 2, 10.1).
///
/// There is no fallback to an earlier save: a damaged end of file is an error,
/// so a truncated file is never read as an older state.
pub(crate) fn current_footer(data: &[u8], first_block: u64) -> Result<u64, NoteError> {
    let len = data.len();
    let first = usize::try_from(first_block).map_err(|_| corrupt("file prefix too large"))?;
    // prefix + one block length field + tail marker
    if len < first + 4 + 8 {
        return Err(corrupt(format!("file too short ({len} bytes)")));
    }
    let marker = len - 8;
    if data.get(marker..marker + 4) != Some(TAIL) {
        return Err(corrupt("missing tail marker at end of file"));
    }
    let address = read_u32(data, marker + 4).ok_or_else(|| corrupt("missing footer address"))?;
    let address = u64::from(address);
    if address < first_block || address >= marker as u64 {
        return Err(corrupt(format!(
            "footer address {address} lies outside [{first_block}, {marker})"
        )));
    }
    let footer =
        payload(data, first_block, address).map_err(|e| corrupt(format!("footer: {e}")))?;
    // address + 4 + len == marker
    if address + 4 + footer.len() as u64 != marker as u64 {
        return Err(corrupt(format!(
            "footer block at {address} does not end at the tail marker"
        )));
    }
    Ok(address)
}

/// Result of reading the file block after block from the first block
/// (docs/note-format.md 2, "Save markers").
#[derive(Debug, Default)]
pub(crate) struct Walk {
    /// `(address, payload length)` in file order.
    pub(crate) blocks: Vec<(u64, usize)>,
    /// Footer addresses of the save markers, in file order.
    pub(crate) footers: Vec<u64>,
    /// Why the walk stopped before the end of the file.
    pub(crate) error: Option<String>,
}

/// Walks all blocks, skipping each 8-byte `tail` + footer address marker.
pub(crate) fn walk(data: &[u8], first_block: u64) -> Walk {
    let mut out = Walk::default();
    let Ok(mut pos) = usize::try_from(first_block) else {
        out.error = Some("file prefix too large".into());
        return out;
    };
    let mut last_block: Option<u64> = None;
    let mut ended_with_marker = false;
    while pos < data.len() {
        // A marker is `tail` followed by the address of the block just read.
        if data.get(pos..pos + 4) == Some(TAIL)
            && let Some(addr) = read_u32(data, pos + 4)
            && Some(u64::from(addr)) == last_block
        {
            out.footers.push(u64::from(addr));
            pos += 8;
            ended_with_marker = true;
            continue;
        }
        let address = pos as u64;
        match payload(data, first_block, address) {
            Ok(body) => {
                out.blocks.push((address, body.len()));
                last_block = Some(address);
                pos += 4 + body.len();
                ended_with_marker = false;
            }
            Err(e) => {
                out.error = Some(e);
                return out;
            }
        }
    }
    if !ended_with_marker {
        out.error = Some("file does not end with a save marker".into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(blocks: &[&[u8]]) -> Vec<u8> {
        let mut data = b"noteSN_FILE_VER_20230015".to_vec();
        let mut last = 0u32;
        for b in blocks {
            last = data.len() as u32;
            data.extend_from_slice(&(b.len() as u32).to_le_bytes());
            data.extend_from_slice(b);
        }
        data.extend_from_slice(b"tail");
        data.extend_from_slice(&last.to_le_bytes());
        data
    }

    #[test]
    fn envelope_variants() {
        let e = envelope(b"noteSN_FILE_VER_20260016rest").unwrap();
        assert_eq!(
            (e.file_type.as_str(), e.version, e.first_block),
            ("note", Some(20260016), 24)
        );
        let e = envelope(b"SN_FILE_ASA_20190529rest").unwrap();
        assert_eq!(
            (e.signature.as_str(), e.first_block),
            ("SN_FILE_ASA_20190529", 20)
        );
        assert!(matches!(
            envelope(b"noteSN_FILE_VER_2026"),
            Err(NoteError::NotANote)
        ));
        assert!(matches!(
            envelope(b"\0\0\0\0SN_FILE_VER_20260016"),
            Err(NoteError::NotANote)
        ));
    }

    #[test]
    fn footer_and_walk() {
        let data = file(&[b"<A:1>", b"\x01\x02", b"<PAGE1:24>"]);
        assert_eq!(current_footer(&data, 24).unwrap(), 24 + 9 + 6);
        let w = walk(&data, 24);
        assert_eq!(w.blocks, vec![(24, 5), (33, 2), (39, 10)]);
        assert_eq!(w.footers, vec![39]);
        assert_eq!(w.error, None);
    }

    #[test]
    fn payload_bounds() {
        let data = file(&[b"<A:1>"]);
        assert_eq!(payload(&data, 24, 24).unwrap(), b"<A:1>");
        assert!(payload(&data, 24, 0).is_err());
        assert!(payload(&data, 24, 4).is_err());
        assert!(payload(&data, 24, data.len() as u64 - 2).is_err());
        assert!(payload(&data, 24, u64::MAX).is_err());
    }
}
