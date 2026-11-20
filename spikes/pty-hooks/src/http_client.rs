//! Minimal HTTP/1.1 client over std `TcpStream` for localhost only.
//! Used by the emitter, FakeCli witnesses, B-8 probes, and `attach-client`.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

/// Parsed `http://127.0.0.1:<port>/<path>` or `http://localhost:<port>/<path>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalUrl {
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl LocalUrl {
    pub fn host_header(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
    pub fn with_path(&self, path: &str) -> LocalUrl {
        LocalUrl {
            path: path.to_string(),
            ..self.clone()
        }
    }
}

/// Accepts exactly `http://127.0.0.1:<port><path>` or `http://localhost:<port><path>`,
/// with a canonical decimal port in 1..=65535 and the given path.
pub fn parse_local_url(url: &str, path: &str) -> Option<LocalUrl> {
    let rest = url.strip_prefix("http://")?;
    let (host, rest) = rest.split_once(':')?;
    if host != "127.0.0.1" && host != "localhost" {
        return None;
    }
    let (port, p) = rest.split_at(rest.find('/')?);
    if p != path || port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if port.starts_with('0') {
        return None;
    }
    let port: u32 = port.parse().ok()?;
    if port == 0 || port > 65535 {
        return None;
    }
    Some(LocalUrl {
        host: host.into(),
        port: port as u16,
        path: path.into(),
    })
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    let now = Instant::now();
    if now >= deadline {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "deadline"));
    }
    Ok(deadline - now)
}

/// Sends one request and returns `(status, body)`. Connects to 127.0.0.1 regardless of
/// the Host header, so `localhost` never resolves to another address.
pub fn request(
    url: &LocalUrl,
    method: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
    connect_timeout: Duration,
    deadline: Instant,
) -> io::Result<(u16, Vec<u8>)> {
    let addr = SocketAddr::from(([127, 0, 0, 1], url.port));
    let ct = connect_timeout.min(remaining(deadline)?);
    let mut s = TcpStream::connect_timeout(&addr, ct)?;
    s.set_nodelay(true).ok();
    let mut head = format!(
        "{method} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        url.path,
        url.host_header()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(b) = body {
        head.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    head.push_str("\r\n");
    write_bounded(&mut s, head.as_bytes(), 64 * 1024, deadline)?;
    if let Some(b) = body {
        write_bounded(&mut s, b, 64 * 1024, deadline)?;
    }
    s.flush()?;
    read_response(&mut s, deadline)
}

/// Writes in pieces of at most `chunk` bytes, each with a write timeout set
/// from the remaining budget. The caller flushes.
pub fn write_bounded(
    s: &mut TcpStream,
    mut bytes: &[u8],
    chunk: usize,
    deadline: Instant,
) -> io::Result<()> {
    while !bytes.is_empty() {
        s.set_write_timeout(Some(remaining(deadline)?))?;
        let n = s.write(&bytes[..bytes.len().min(chunk)])?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0"));
        }
        bytes = &bytes[n..];
    }
    Ok(())
}

pub fn read_response(s: &mut TcpStream, deadline: Instant) -> io::Result<(u16, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        s.set_read_timeout(Some(remaining(deadline)?))?;
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset && !buf.is_empty() => break,
            Err(e) => return Err(e),
        }
        if buf.len() > 8 << 20 {
            return Err(io::Error::other("response too large"));
        }
    }
    let mut hdrs = [httparse::EMPTY_HEADER; 32];
    let mut resp = httparse::Response::new(&mut hdrs);
    match resp.parse(&buf) {
        Ok(httparse::Status::Complete(n)) => Ok((resp.code.unwrap_or(0), buf[n..].to_vec())),
        _ => Err(io::Error::other("malformed response")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_url_is_exact() {
        assert!(parse_local_url("http://127.0.0.1:8080/hook", "/hook").is_some());
        assert!(parse_local_url("http://localhost:1/hook", "/hook").is_some());
        for bad in [
            "https://127.0.0.1:8080/hook",
            "http://127.0.0.2:8080/hook",
            "http://127.0.0.1/hook",
            "http://127.0.0.1:/hook",
            "http://127.0.0.1:0/hook",
            "http://127.0.0.1:08080/hook",
            "http://127.0.0.1:65536/hook",
            "http://127.0.0.1:80a/hook",
            "http://127.0.0.1:8080/hook/",
            "http://127.0.0.1:8080/hook?x",
            "http://user@127.0.0.1:8080/hook",
            "http://[::1]:8080/hook",
            "http://LOCALHOST:8080/hook",
            " http://127.0.0.1:8080/hook",
        ] {
            assert!(parse_local_url(bad, "/hook").is_none(), "{bad}");
        }
    }
}
