//! The emitter's environment (hook-ingest §2.13): `CHALDEA_HOOK_URL`, `CHALDEA_HOOK_TOKEN`,
//! and `CHALDEA_THREAD_ID`, and no other variable.

use std::ffi::OsString;

pub(crate) const URL: &str = "CHALDEA_HOOK_URL";
pub(crate) const TOKEN: &str = "CHALDEA_HOOK_TOKEN";
pub(crate) const THREAD: &str = "CHALDEA_THREAD_ID";

/// The base64url characters of a token's secret (`chd_` + 43).
const SECRET_LEN: usize = 43;

/// What the emitter sends with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Env {
    /// `127.0.0.1` or `localhost`, as written in the URL: the `Host` header's name. The
    /// connection always goes to `127.0.0.1`.
    pub(crate) host: &'static str,
    pub(crate) port: u16,
    pub(crate) token: String,
    /// `X-Chaldea-Thread-Id`, when the variable is set to a header-safe value.
    pub(crate) thread: Option<String>,
}

/// The environment of this process; `None` when the URL or the token is missing or not
/// exact.
pub(crate) fn read() -> Option<Env> {
    from(|k| std::env::var_os(k))
}

/// [`read`] over a variable lookup, which is called with [`URL`], [`TOKEN`], and [`THREAD`]
/// only.
pub(crate) fn from(get: impl Fn(&str) -> Option<OsString>) -> Option<Env> {
    let url = get(URL)?.into_string().ok()?;
    let (host, port) = parse_url(&url)?;
    let token = get(TOKEN)?.into_string().ok()?;
    if !is_token(&token) {
        return None;
    }
    let thread = get(THREAD)
        .and_then(|t| t.into_string().ok())
        .filter(|t| header_safe(t));
    Some(Env {
        host,
        port,
        token,
        thread,
    })
}

/// Exactly `http://127.0.0.1:<port>/hook` or `http://localhost:<port>/hook`, the port
/// decimal in 1–65535 with no leading zero.
pub(crate) fn parse_url(url: &str) -> Option<(&'static str, u16)> {
    let rest = url.strip_prefix("http://")?;
    let (host, rest) = rest.split_once(':')?;
    let host = match host {
        "127.0.0.1" => "127.0.0.1",
        "localhost" => "localhost",
        _ => return None,
    };
    let port = rest.strip_suffix("/hook")?;
    if port.is_empty()
        || port.len() > 5
        || port.starts_with('0')
        || !port.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let port: u16 = port.parse().ok()?;
    Some((host, port))
}

/// `chd_` and 43 base64url characters.
pub(crate) fn is_token(t: &str) -> bool {
    t.strip_prefix("chd_").is_some_and(|s| {
        s.len() == SECRET_LEN
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    })
}

/// A non-empty value of visible ASCII only (no space, no control character), so it cannot
/// change the request's framing.
pub(crate) fn header_safe(v: &str) -> bool {
    !v.is_empty() && v.bytes().all(|b| b.is_ascii_graphic())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token() -> String {
        format!("chd_{}", "aZ09-_".repeat(7) + "x")
    }

    #[test]
    fn url_is_exact() {
        assert_eq!(
            parse_url("http://127.0.0.1:8080/hook"),
            Some(("127.0.0.1", 8080))
        );
        assert_eq!(parse_url("http://localhost:1/hook"), Some(("localhost", 1)));
        assert_eq!(
            parse_url("http://127.0.0.1:65535/hook"),
            Some(("127.0.0.1", 65535))
        );
        for bad in [
            "",
            "https://127.0.0.1:8080/hook",
            "http://127.0.0.2:8080/hook",
            "http://127.0.0.1/hook",
            "http://127.0.0.1:/hook",
            "http://127.0.0.1:0/hook",
            "http://127.0.0.1:08080/hook",
            "http://127.0.0.1:65536/hook",
            "http://127.0.0.1:123456/hook",
            "http://127.0.0.1:80a/hook",
            "http://127.0.0.1:+80/hook",
            "http://127.0.0.1:8080/hook/",
            "http://127.0.0.1:8080/hooks",
            "http://127.0.0.1:8080/hook?x",
            "http://127.0.0.1:8080/hook#x",
            "http://127.0.0.1:8080",
            "http://user@127.0.0.1:8080/hook",
            "http://[::1]:8080/hook",
            "http://LOCALHOST:8080/hook",
            " http://127.0.0.1:8080/hook",
            "http://127.0.0.1:8080/hook ",
            "HTTP://127.0.0.1:8080/hook",
        ] {
            assert_eq!(parse_url(bad), None, "{bad}");
        }
    }

    #[test]
    fn token_form() {
        assert!(is_token(&token()));
        for bad in [
            String::new(),
            "chd_".to_owned(),
            format!("chd_{}", "a".repeat(42)),
            format!("chd_{}", "a".repeat(44)),
            format!("chd_{}!", "a".repeat(42)),
            format!("chd_{}=", "a".repeat(42)),
            format!("chdt_{}", "a".repeat(43)),
            format!("Chd_{}", "a".repeat(43)),
            format!(" chd_{}", "a".repeat(43)),
        ] {
            assert!(!is_token(&bad), "{bad}");
        }
    }

    #[test]
    fn reads_only_its_variables() {
        let asked = std::cell::RefCell::new(Vec::new());
        let t = token();
        let env = from(|k| {
            asked.borrow_mut().push(k.to_owned());
            match k {
                URL => Some("http://localhost:4000/hook".into()),
                TOKEN => Some(t.clone().into()),
                THREAD => Some("0190-thread".into()),
                _ => Some("other".into()),
            }
        });
        assert_eq!(
            env,
            Some(Env {
                host: "localhost",
                port: 4000,
                token: t.clone(),
                thread: Some("0190-thread".to_owned()),
            })
        );
        assert_eq!(*asked.borrow(), [URL, TOKEN, THREAD]);
        // A thread id that is not header-safe is not sent; the rest stands.
        for bad in ["", "a b", "a\r\nX: 1", "é"] {
            let env = from(|k| match k {
                URL => Some("http://127.0.0.1:4000/hook".into()),
                TOKEN => Some(t.clone().into()),
                THREAD => Some(bad.into()),
                _ => None,
            })
            .unwrap();
            assert_eq!(env.thread, None, "{bad:?}");
        }
    }

    #[test]
    fn missing_or_malformed_url_or_token_is_none() {
        let t = token();
        let with = |url: Option<&str>, tok: Option<&str>| {
            from(|k| match k {
                URL => url.map(Into::into),
                TOKEN => tok.map(Into::into),
                THREAD => Some("t".into()),
                _ => None,
            })
        };
        assert!(with(Some("http://127.0.0.1:1/hook"), Some(&t)).is_some());
        assert!(with(None, Some(&t)).is_none());
        assert!(with(Some("http://127.0.0.1:1/hook"), None).is_none());
        assert!(with(Some("http://127.0.0.1:1/hook/"), Some(&t)).is_none());
        assert!(with(Some("http://127.0.0.1:1/hook"), Some("chd_short")).is_none());
        assert!(with(None, None).is_none());
    }
}
