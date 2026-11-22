//! Secrets in logs (design §2.10): the request span records the method (a fixed label), the
//! route template,
//! the status, the latency, and the token id; never the URI, the query, header values, or
//! bodies. [`redact_secrets`] is the second line PR 8's log formatter applies.

use tracing::Span;
use tracing::field::Empty;

/// The prefixes of the daemon's secrets: tokens, tickets, pair codes.
const PREFIXES: [&str; 3] = ["chdt_", "chdp_", "chd_"];

/// The route field of a request that matched no route.
pub(crate) const UNMATCHED: &str = "unmatched";

/// The method as recorded: the standard methods by name, any extension method as `other`
/// (an extension method is client text, like a header value).
fn method_label(method: &axum::http::Method) -> &'static str {
    match method.as_str() {
        "GET" => "GET",
        "HEAD" => "HEAD",
        "POST" => "POST",
        "PUT" => "PUT",
        "DELETE" => "DELETE",
        "CONNECT" => "CONNECT",
        "OPTIONS" => "OPTIONS",
        "TRACE" => "TRACE",
        "PATCH" => "PATCH",
        _ => "other",
    }
}

/// Opens the request span: one per request, outermost.
pub(crate) fn request_span(method: &axum::http::Method) -> Span {
    tracing::info_span!(
        "request",
        method = method_label(method),
        route = Empty,
        status = Empty,
        latency_ms = Empty,
        token_id = Empty,
    )
}

fn is_secret_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Replaces every `chd_`, `chdt_`, and `chdp_` secret (the prefix followed by base64url
/// characters) in `s` with the prefix and `<redacted>`.
#[must_use]
pub fn redact_secrets(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut copied = 0;
    while i < bytes.len() {
        let matched = PREFIXES
            .iter()
            .find(|p| bytes[i..].starts_with(p.as_bytes()));
        if let Some(p) = matched {
            let start = i + p.len();
            let mut end = start;
            while end < bytes.len() && is_secret_char(bytes[end]) {
                end += 1;
            }
            if end > start {
                out.push_str(&s[copied..i]);
                out.push_str(p);
                out.push_str("<redacted>");
                copied = end;
                i = end;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&s[copied..]);
    out
}

#[cfg(test)]
mod tests {
    use super::redact_secrets;

    #[test]
    fn redact_secrets_replaces_every_pattern() {
        let t = format!("chd_{}", "A".repeat(43));
        let k = format!("chdt_{}", "b-_9".repeat(10));
        let p = "chdp_x";
        let line = format!("token={t} ticket={k}&x=1 code:{p}. chd_ alone, achd_zz");
        let r = redact_secrets(&line);
        assert_eq!(
            r,
            "token=chd_<redacted> ticket=chdt_<redacted>&x=1 code:chdp_<redacted>. chd_ alone, achd_<redacted>"
        );
        assert!(!r.contains(&t) && !r.contains(&k));
        assert_eq!(
            redact_secrets("日本語 chd_abc 終"),
            "日本語 chd_<redacted> 終"
        );
        assert_eq!(redact_secrets("no secrets"), "no secrets");
    }
}
