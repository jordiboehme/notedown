//! The password check and the login page.

use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use notedown_cloud::auth::{Account, Accounts, md5_hex};
use subtle::ConstantTimeEq;

/// The hash an unknown email is compared with, so it costs the same work as a wrong password.
const NO_ACCOUNT_MD5: &str = "00000000000000000000000000000000";

/// The account of `email` when `password` is its device password. Never changes the account.
pub(crate) fn check_password(
    accounts: &Accounts,
    email: &str,
    password: &str,
) -> anyhow::Result<Option<Account>> {
    let account = accounts.find_by_email(email.trim())?;
    let expected = account
        .as_ref()
        .map_or(NO_ACCOUNT_MD5, |a| a.password_md5.as_str());
    let supplied = md5_hex(password);
    let same: bool = supplied
        .as_bytes()
        .ct_eq(expected.to_ascii_lowercase().as_bytes())
        .into();
    Ok(if same { account } else { None })
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// The CSP of a page without a form.
const CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

/// The CSP of the login page. Chrome checks the redirect after a form post against `form-action`,
/// so the origin of the redirect URI is listed.
fn login_csp(redirect_origin: &str) -> String {
    format!(
        "default-src 'none'; style-src 'unsafe-inline'; form-action 'self' {redirect_origin}; frame-ancestors 'none'; base-uri 'none'"
    )
}

const STYLE: &str = "body{font-family:system-ui,sans-serif;max-width:28rem;margin:3rem auto;padding:0 1rem;color:#222;background:#fafafa}\
h1{font-size:1.4rem}label{display:block;margin-top:1rem}input{width:100%;padding:.5rem;font-size:1rem;box-sizing:border-box}\
.row{display:flex;gap:.5rem;margin-top:1.5rem}button{flex:1;padding:.6rem;font-size:1rem}\
.warn{background:#fff3cd;padding:.6rem;border-radius:4px}.err{background:#f8d7da;padding:.6rem;border-radius:4px}\
small{color:#555}";

fn page(status: StatusCode, csp: &str, title: &str, body: &str) -> Response {
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>{title}</title><style>{STYLE}</style></head><body>{body}</body></html>"
    );
    (
        status,
        [
            (header::CONTENT_SECURITY_POLICY, csp),
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::X_FRAME_OPTIONS, "DENY"),
        ],
        Html(html),
    )
        .into_response()
}

pub(crate) struct Page {
    pub client_name: String,
    pub client_uri: Option<String>,
    pub redirect_host: String,
    /// Scheme and authority of the redirect URI, for example `https://claude.ai`.
    pub redirect_origin: String,
    pub loopback: bool,
    pub pending_id: String,
    pub minutes_left: u64,
    pub error: Option<String>,
    pub email: String,
}

pub(crate) fn render(p: &Page) -> Response {
    let mut body = format!(
        "<h1>Sign in to notedown</h1><p><b>{}</b> wants to read your notes and move its \
bookmarks.</p>",
        escape(&p.client_name)
    );
    if let Some(uri) = &p.client_uri {
        body.push_str(&format!("<p><small>Client: {}</small></p>", escape(uri)));
    }
    body.push_str(&format!(
        "<p><small>After the sign-in you go back to: {}</small></p>",
        escape(&p.redirect_host)
    ));
    if p.loopback {
        body.push_str(
            "<p class=\"warn\">This client runs on this computer. Sign in only if \
you started it yourself.</p>",
        );
    }
    if let Some(e) = &p.error {
        body.push_str(&format!("<p class=\"err\">{}</p>", escape(e)));
    }
    body.push_str(&format!(
        "<form method=\"post\" action=\"/oauth/authorize\">\
<input type=\"hidden\" name=\"pending\" value=\"{}\">\
<label>Email<input name=\"email\" type=\"email\" autocomplete=\"username\" required value=\"{}\"></label>\
<label>Password<input name=\"password\" type=\"password\" autocomplete=\"current-password\" required></label>\
<div class=\"row\"><button name=\"action\" value=\"login\" type=\"submit\">Sign in</button>\
<button name=\"action\" value=\"cancel\" type=\"submit\" formnovalidate>Cancel</button></div></form>\
<p><small>Use the email and password of your notedown account. This request is valid for {} more \
minutes.</small></p>",
        escape(&p.pending_id),
        escape(&p.email),
        p.minutes_left
    ));
    page(
        StatusCode::OK,
        &login_csp(&p.redirect_origin),
        "Sign in to notedown",
        &body,
    )
}

pub(crate) fn error_page(status: StatusCode, message: &str) -> Response {
    page(
        status,
        CSP,
        "notedown sign-in",
        &format!(
            "<h1>Sign-in not possible</h1><p class=\"err\">{}</p>",
            escape(message)
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use notedown_cloud::auth::Accounts;

    #[test]
    fn password_check_is_case_insensitive_on_email_and_leaves_the_device_lockout() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = Accounts::new(dir.path());
        accounts
            .create("anna", "Anna@Example.com", "right")
            .unwrap();
        assert_eq!(
            check_password(&accounts, "anna@example.com", "right")
                .unwrap()
                .unwrap()
                .slug,
            "anna"
        );
        for _ in 0..10 {
            assert!(
                check_password(&accounts, "anna@example.com", "wrong")
                    .unwrap()
                    .is_none()
            );
        }
        assert!(
            check_password(&accounts, "nobody@example.com", "right")
                .unwrap()
                .is_none()
        );
        let a = accounts.get("anna").unwrap().unwrap();
        assert_eq!(a.failed_logins, 0);
        assert!(!a.locked);
    }

    #[tokio::test]
    async fn the_page_escapes_and_sets_strict_headers() {
        let r = render(&Page {
            client_name: "<script>x</script>".into(),
            client_uri: Some("https://claude.ai".into()),
            redirect_host: "127.0.0.1:9".into(),
            redirect_origin: "https://claude.ai".into(),
            loopback: true,
            pending_id: "p1".into(),
            minutes_left: 10,
            error: None,
            email: "a\"><b>".into(),
        });
        let csp = r.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .to_string();
        assert!(csp.contains("frame-ancestors 'none'") && csp.contains("default-src 'none'"));
        assert!(csp.contains("form-action 'self' https://claude.ai;"));
        assert_eq!(r.headers()["cache-control"], "no-store");
        let body = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(!html.contains("<script>"));
        assert!(html.contains("a&quot;&gt;&lt;b&gt;") && !html.contains("a\"><b>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("name=\"pending\" value=\"p1\""));
        assert!(html.contains("this computer"), "loopback warning");
        assert!(!html.contains('\u{2014}'), "no em-dash");
    }
}
