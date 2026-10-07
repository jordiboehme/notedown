//! Signed transfer URLs for `/api/oss/*` (`docs/sync-protocol.md` 3.5, 3.9).
//!
//! The device never computes a signature; it uses the URLs as given. notedown signs with its own
//! key `keys/url.key` (32 random bytes, hex, mode 0600, created on the first start):
//!
//! ```text
//! signature = hex(HMAC-SHA256(url.key, "<method>|<path_b64>|<timestamp>|<nonce>|<purpose>|<size>"))
//! ```
//!
//! - `method`: `GET` for downloads, `POST` for uploads.
//! - `path_b64`: the `path` query value, base64url without padding of a [`SignedTarget`]
//!   (account, device, token generation, wire path, file id or `innerName`, size). Transfers
//!   carry no token, so everything they act on is inside the signed value.
//! - `timestamp`: ms since the epoch at signing; `nonce`: 32 random hex characters.
//! - `purpose`: `download` or `upload`; `size`: the byte count the URL is for (`0` for
//!   downloads).
//!
//! Downloads are valid for 24 h and uploads for 30 min. Nonces are not tracked (a download may
//! be repeated, for example with a `Range`).

use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::fsutil::write_atomic;
use crate::{Error, Result, io_err};

/// `<data>/keys/url.key`.
pub const URL_KEY_FILE: &str = "url.key";
/// How long a download URL is valid (ms).
pub const DOWNLOAD_TTL_MS: u64 = 24 * 60 * 60 * 1000;
/// How long an upload URL is valid (ms).
pub const UPLOAD_TTL_MS: u64 = 30 * 60 * 1000;
/// A timestamp this far in the future (clock skew between restarts) is still accepted (ms).
pub const MAX_FUTURE_SKEW_MS: u64 = 5 * 60 * 1000;
/// The error text of a failed signature check (HTTP 500, plain text, as Ratta answers).
pub const E_SIGNATURE: &str = "E1306";
/// The error text of a download whose file cannot be served (HTTP 500, plain text).
pub const E_DOWNLOAD: &str = "E1307";
/// The error text of an upload that failed on the server side (HTTP 500, plain text).
pub const E_UPLOAD_FAILED: &str = "E1305";

type HmacSha256 = Hmac<Sha256>;

/// What a signed URL is for; part of the signed text, so one kind cannot stand in for the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Download,
    Upload,
}

impl Purpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Purpose::Download => "download",
            Purpose::Upload => "upload",
        }
    }

    /// The validity of a URL of this purpose (ms).
    pub fn ttl_ms(self) -> u64 {
        match self {
            Purpose::Download => DOWNLOAD_TTL_MS,
            Purpose::Upload => UPLOAD_TTL_MS,
        }
    }
}

/// The signed query values of one URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signed {
    pub signature: String,
    pub timestamp: u64,
    pub nonce: String,
}

/// Why a signed URL was refused. Both answer `E1306`; the difference is for the log.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UrlError {
    #[error("bad or missing signature")]
    Bad,
    #[error("signature expired")]
    Expired,
}

/// The HMAC key of signed URLs.
#[derive(Clone)]
pub struct UrlSigner {
    key: Vec<u8>,
}

impl std::fmt::Debug for UrlSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UrlSigner(<masked>)")
    }
}

fn b64url_ok(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn hex_ok(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl UrlSigner {
    pub fn from_bytes(key: Vec<u8>) -> UrlSigner {
        UrlSigner { key }
    }

    /// Load `<data>/keys/url.key` (64 hex characters); generate it with mode 0600 when missing.
    pub fn load_or_create(data_dir: &Path) -> Result<UrlSigner> {
        let path = data_dir.join("keys").join(URL_KEY_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let key = const_hex::decode(text.trim()).map_err(|_| Error::Config {
                    path: path.clone(),
                    message: "not a hex key".into(),
                })?;
                if key.len() < 32 {
                    return Err(Error::Config {
                        path,
                        message: "key shorter than 32 bytes".into(),
                    });
                }
                Ok(UrlSigner { key })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut key = vec![0u8; 32];
                rand::fill(&mut key[..]);
                let text = format!("{}\n", const_hex::encode(&key));
                write_atomic(&path, text.as_bytes(), true)?;
                Ok(UrlSigner { key })
            }
            Err(e) => Err(io_err(&path)(e)),
        }
    }

    fn mac(
        &self,
        method: &str,
        path_b64: &str,
        timestamp: u64,
        nonce: &str,
        purpose: Purpose,
        size: u64,
    ) -> HmacSha256 {
        let mut mac = <HmacSha256 as KeyInit>::new_from_slice(&self.key)
            .expect("hmac accepts any key length");
        mac.update(
            format!(
                "{method}|{path_b64}|{timestamp}|{nonce}|{}|{size}",
                purpose.as_str()
            )
            .as_bytes(),
        );
        mac
    }

    /// Sign a URL at `now_ms` with a fresh nonce.
    pub fn sign(
        &self,
        method: &str,
        path_b64: &str,
        purpose: Purpose,
        size: u64,
        now_ms: u64,
    ) -> Signed {
        let mut raw = [0u8; 16];
        rand::fill(&mut raw[..]);
        let nonce = const_hex::encode(raw);
        let signature = const_hex::encode(
            self.mac(method, path_b64, now_ms, &nonce, purpose, size)
                .finalize()
                .into_bytes(),
        );
        Signed {
            signature,
            timestamp: now_ms,
            nonce,
        }
    }

    /// Check the query values of a signed URL at `now_ms` (constant-time comparison). Malformed
    /// values are [`UrlError::Bad`]; a valid signature past its validity is
    /// [`UrlError::Expired`].
    #[allow(clippy::too_many_arguments)]
    pub fn verify(
        &self,
        method: &str,
        path_b64: &str,
        timestamp: &str,
        nonce: &str,
        signature: &str,
        purpose: Purpose,
        size: u64,
        now_ms: u64,
    ) -> std::result::Result<(), UrlError> {
        if !b64url_ok(path_b64)
            || !hex_ok(nonce, 32)
            || !hex_ok(signature, 64)
            || timestamp.is_empty()
            || timestamp.len() > 19
            || !timestamp.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(UrlError::Bad);
        }
        let ts: u64 = timestamp.parse().map_err(|_| UrlError::Bad)?;
        let expected = self
            .mac(method, path_b64, ts, nonce, purpose, size)
            .finalize()
            .into_bytes();
        let supplied = const_hex::decode(signature).map_err(|_| UrlError::Bad)?;
        if !bool::from(expected.as_slice().ct_eq(&supplied)) {
            return Err(UrlError::Bad);
        }
        if ts > now_ms.saturating_add(MAX_FUTURE_SKEW_MS)
            || now_ms.saturating_sub(ts) > purpose.ttl_ms()
        {
            return Err(UrlError::Expired);
        }
        Ok(())
    }
}

/// What a signed URL acts on, encoded as the URL's `path` value (so all of it is signed):
///
/// - `account`, `device`: the account and the device the URL was issued to (the GET or the
///   upload carries no token; the device's sync lock is checked and extended with this serial).
/// - `generation`: the device's token generation at signing. A logout or a new login raises it,
///   and the URL stops working (`E1306`).
/// - `path`: downloads: the file's wire path; uploads: the target's parent folder.
/// - `id`: downloads: the file id, which must still resolve to `path` at GET time (`E1307`).
/// - `inner`: uploads: the `innerName` (a UUID) that names the staging entry.
/// - `size`: uploads: the size announced in `upload/apply` (also the signer's `size`); `0` for
///   downloads.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SignedTarget {
    pub account: String,
    pub device: String,
    pub generation: u64,
    pub path: String,
    pub id: Option<u64>,
    pub inner: Option<String>,
    pub size: u64,
}

type Wire = (
    String,
    String,
    u64,
    String,
    Option<u64>,
    Option<String>,
    u64,
);

impl SignedTarget {
    /// base64url without padding of the JSON array
    /// `[account, device, generation, path, id, inner, size]`.
    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(&(
            &self.account,
            &self.device,
            self.generation,
            &self.path,
            self.id,
            &self.inner,
            self.size,
        ))
        .expect("plain values serialize");
        URL_SAFE_NO_PAD.encode(json)
    }

    /// The inverse of [`SignedTarget::encode`]; `None` for anything else.
    pub fn decode(path_b64: &str) -> Option<SignedTarget> {
        let bytes = URL_SAFE_NO_PAD.decode(path_b64).ok()?;
        let (account, device, generation, path, id, inner, size): Wire =
            serde_json::from_slice(&bytes).ok()?;
        Some(SignedTarget {
            account,
            device,
            generation,
            path,
            id,
            inner,
            size,
        })
    }
}

/// A `Host` header value that is safe to put into a URL: a host name or address with an
/// optional port, nothing else.
fn host_ok(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 255
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'))
}

/// The base URL the device must use: `external_base_url` without a trailing slash when it is
/// set, else `http://<Host header>`. Without a usable `Host` header: `http://localhost`.
pub fn base_url(external_base_url: Option<&str>, host: Option<&str>) -> String {
    if let Some(base) = external_base_url.map(str::trim).filter(|b| !b.is_empty()) {
        return base.trim_end_matches('/').to_string();
    }
    match host.map(str::trim) {
        Some(h) if host_ok(h) => format!("http://{h}"),
        _ => "http://localhost".to_string(),
    }
}

/// `<base>/api/oss/<endpoint>?path=..&signature=..&timestamp=..&nonce=..` (`endpoint` is
/// `upload`, `upload/part`, `upload/summary` or `download/summary`; the device appends
/// `&uploadId=..&partNumber=..&totalChunks=..` to the part URL). Every value is URL-safe as it
/// is.
pub fn oss_url(base: &str, endpoint: &str, path_b64: &str, signed: &Signed) -> String {
    format!(
        "{base}/api/oss/{endpoint}?path={path_b64}&signature={}&timestamp={}&nonce={}",
        signed.signature, signed.timestamp, signed.nonce
    )
}

/// `<base>/api/oss/download?path=..&signature=..&timestamp=..&nonce=..&pathId=..`. Every value
/// is URL-safe as it is (base64url, hex, digits).
pub fn download_url(base: &str, path_b64: &str, signed: &Signed, path_id: u64) -> String {
    format!(
        "{base}/api/oss/download?path={path_b64}&signature={}&timestamp={}&nonce={}&pathId={path_id}",
        signed.signature, signed.timestamp, signed.nonce
    )
}

/// `Content-Disposition: attachment; filename*=UTF-8''<name>` (RFC 5987): every byte outside
/// the unreserved set (`A-Z a-z 0-9 - . _ ~`) is percent-encoded.
pub fn content_disposition(name: &str) -> String {
    let mut out = String::from("attachment; filename*=UTF-8''");
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
