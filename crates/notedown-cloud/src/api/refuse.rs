//! Safety: the capture server never lists files and never accepts a file operation. Every path
//! under `/api/file/` (except `query/server`) and `/api/oss/` answers `E0079`, whatever the method,
//! the body or the token. Nothing here reads or writes the data folder.

use axum::http::{Method, Uri};

use super::envelope::Reply;

/// Documented file, digest, task, oss, recycle, share and convert endpoints
/// (`docs/sync-protocol.md` 3.4, 3.5, 3.8, 5.3, 6.1, 6.3 to 6.6, 6.8). A path not in this list is
/// refused just the same and flagged `unknown_endpoint`. `{}` matches one path segment.
const DOCUMENTED: &[(&str, &str)] = &[
    // 3.4, 3.5
    ("POST", "/api/file/2/files/synchronous/start"),
    ("POST", "/api/file/2/files/synchronous/end"),
    ("POST", "/api/file/2/files/list_folder"),
    ("POST", "/api/file/3/files/list_folder_v3"),
    ("POST", "/api/file/3/files/query/by/path_v3"),
    ("POST", "/api/file/3/files/query_v3"),
    ("POST", "/api/file/2/files/create_folder_v2"),
    ("POST", "/api/file/3/files/upload/apply"),
    ("POST", "/api/oss/upload"),
    ("POST", "/api/oss/upload/part"),
    ("POST", "/api/file/2/files/upload/finish"),
    ("POST", "/api/file/3/files/download_v3"),
    ("GET", "/api/oss/download"),
    ("POST", "/api/file/3/files/delete_folder_v3"),
    ("POST", "/api/file/3/files/move_v3"),
    ("POST", "/api/file/3/files/copy_v3"),
    ("POST", "/api/file/2/users/get_space_usage"),
    ("POST", "/api/file/capacity/query"),
    // 3.8
    ("POST", "/api/file/terminal/upload/apply"),
    ("POST", "/api/file/terminal/upload/finish"),
    ("POST", "/api/file/upload/apply"),
    ("POST", "/api/file/upload/finish"),
    // 5.3
    ("POST", "/api/file/add/summary"),
    ("PUT", "/api/file/update/summary"),
    ("DELETE", "/api/file/delete/summary"),
    ("POST", "/api/file/query/summary"),
    ("POST", "/api/file/query/summary/hash"),
    ("POST", "/api/file/query/summary/id"),
    ("POST", "/api/file/add/summary/group"),
    ("PUT", "/api/file/update/summary/group"),
    ("DELETE", "/api/file/delete/summary/group"),
    ("POST", "/api/file/query/summary/group"),
    ("POST", "/api/file/add/summary/tag"),
    ("PUT", "/api/file/update/summary/tag"),
    ("DELETE", "/api/file/delete/summary/tag"),
    ("GET", "/api/file/query/summary/tag"),
    ("POST", "/api/file/upload/apply/summary"),
    ("POST", "/api/file/download/summary"),
    // 6.1
    ("POST", "/api/file/schedule/group/all"),
    ("POST", "/api/file/schedule/group"),
    ("PUT", "/api/file/schedule/group"),
    ("DELETE", "/api/file/schedule/group/{}"),
    ("POST", "/api/file/schedule/group/clear"),
    ("GET", "/api/file/schedule/group/{}"),
    ("POST", "/api/file/schedule/task/all"),
    ("POST", "/api/file/schedule/task"),
    ("PUT", "/api/file/schedule/task"),
    ("PUT", "/api/file/schedule/task/list"),
    ("DELETE", "/api/file/schedule/task/{}"),
    ("GET", "/api/file/schedule/task/{}"),
    ("POST", "/api/file/schedule/sort"),
    ("PUT", "/api/file/schedule/sort"),
    ("DELETE", "/api/file/schedule/sort/{}"),
    ("POST", "/api/file/query/schedule/sort"),
    // 6.4 to 6.6
    ("POST", "/api/file/recycle/list/query"),
    ("POST", "/api/file/recycle/clear"),
    ("POST", "/api/file/recycle/delete"),
    ("POST", "/api/file/recycle/revert"),
    ("POST", "/api/file/share/record/add"),
    ("POST", "/api/file/note/to/pdf"),
    ("POST", "/api/file/note/to/png"),
    ("POST", "/api/file/pdfwithmark/to/pdf"),
    // 6.8 (web file actions)
    ("POST", "/api/file/list/query"),
    ("POST", "/api/file/folder/list/query"),
    ("POST", "/api/file/delete"),
    ("POST", "/api/file/folder/add"),
    ("POST", "/api/file/move"),
    ("POST", "/api/file/copy"),
    ("POST", "/api/file/rename"),
    ("POST", "/api/file/list/search"),
    ("POST", "/api/file/path/query"),
    ("POST", "/api/file/download/url"),
];

/// Decode `%XX` escapes of a path (`+` stays as it is). Invalid escapes are kept.
fn percent_decode_path(path: &str) -> String {
    let b = path.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = b
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-decode, drop `;params` of a segment, resolve `.` and `..` segments, collapse repeated
/// slashes and lowercase, so `//API/./%66ile;v=1/x` cannot slip past the refusal.
pub fn normalize(path: &str) -> String {
    let path = percent_decode_path(path);
    let mut segs: Vec<String> = Vec::new();
    for seg in path.split('/') {
        let seg = seg.split(';').next().unwrap_or("");
        match seg {
            "" | "." => {}
            ".." => {
                segs.pop();
            }
            s => segs.push(s.to_ascii_lowercase()),
        }
    }
    if segs.is_empty() {
        return "/".to_string();
    }
    segs.iter().map(|s| format!("/{s}")).collect()
}

/// Whether a (normalized) path belongs to the refused families: `/api/file`, `/api/oss`, and the
/// same without `/api` (as a backend behind a proxy that strips the prefix would see them).
pub fn is_refused_path(normalized: &str) -> bool {
    ["/api/file", "/api/oss", "/file", "/oss"]
        .iter()
        .any(|p| normalized == *p || normalized.starts_with(&format!("{p}/")))
}

fn matches(pattern: &str, path: &str) -> bool {
    let mut p = pattern.split('/');
    let mut q = path.split('/');
    loop {
        match (p.next(), q.next()) {
            (None, None) => return true,
            (Some("{}"), Some(seg)) if !seg.is_empty() => {}
            (Some(a), Some(b)) if a == b => {}
            _ => return false,
        }
    }
}

pub fn is_documented(method: &Method, path: &str) -> bool {
    DOCUMENTED
        .iter()
        .any(|(m, p)| method.as_str() == *m && matches(p, path))
}

/// The answer for every refused request. Documented endpoints are not flagged; anything else in
/// these families is flagged `unknown_endpoint`.
pub fn refusal(method: &Method, uri: &Uri) -> Reply {
    let reply = Reply::sync_busy();
    if is_documented(method, uri.path()) {
        reply
    } else {
        reply.flag(crate::api::FLAG_UNKNOWN)
    }
}

pub async fn refuse(method: Method, uri: Uri) -> Reply {
    refusal(&method, &uri)
}
