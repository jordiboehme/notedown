//! Every request no route takes: logged with `unknown_endpoint` (by the request log, from the
//! flag set here) and answered with an error envelope. Paths of the file families still get
//! `E0079`, so a spelling the router does not know cannot reach a file operation.

use axum::http::{Method, Uri};

use super::envelope::{E_GENERIC, MSG_UNKNOWN, Reply};
use super::refuse::{is_refused_path, normalize};

pub async fn catch_all(method: Method, uri: Uri) -> Reply {
    let _ = method;
    let reply = if is_refused_path(&normalize(uri.path())) {
        Reply::sync_busy()
    } else {
        Reply::error(E_GENERIC, MSG_UNKNOWN)
    };
    reply.flag(crate::api::FLAG_UNKNOWN)
}
