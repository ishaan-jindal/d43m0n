//! Minimal HTTP/1.1 JSON client over std TcpStream, for a local Ollama.
//!
//! Plain `http://` only: no TLS, redirects, proxies or chunked bodies. Those
//! bail instead of guessing, which keeps the dependency list short.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::Value;

/// POST a JSON body and decode the JSON response.
pub fn post_json(url: &str, body: &Value, timeout: Duration) -> Result<Value> {
    let payload = serde_json::to_vec(body).context("encoding request JSON")?;
    request(url, "POST", Some(&payload), timeout)
}

/// GET and decode a JSON response.
pub fn get_json(url: &str, timeout: Duration) -> Result<Value> {
    request(url, "GET", None, timeout)
}

struct Target {
    host: String,
    port: u16,
    path: String,
}

impl Target {
    fn parse(url: &str) -> Result<Self> {
        let rest = url
            .strip_prefix("http://")
            .with_context(|| format!("unsupported URL (http:// only): {url}"))?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if authority.is_empty() {
            bail!("missing host in URL: {url}");
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (
                host.to_string(),
                port.parse()
                    .with_context(|| format!("bad port in URL: {url}"))?,
            ),
            None => (authority.to_string(), 80),
        };
        Ok(Self {
            host,
            port,
            path: path.to_string(),
        })
    }
}

fn connect(host: &str, port: u16, timeout: Duration) -> Result<TcpStream> {
    let addrs = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("resolving {host}:{port}"))?;
    let mut last_err = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => return Ok(stream),
            Err(e) => last_err = Some(e),
        }
    }
    match last_err {
        Some(e) => Err(e).with_context(|| format!("connecting to {host}:{port}")),
        None => bail!("no addresses for {host}:{port}"),
    }
}

fn request(url: &str, method: &str, body: Option<&[u8]>, timeout: Duration) -> Result<Value> {
    let target = Target::parse(url)?;
    let mut stream = connect(&target.host, target.port, timeout)?;
    stream
        .set_read_timeout(Some(timeout))
        .context("setting read timeout")?;
    stream
        .set_write_timeout(Some(timeout))
        .context("setting write timeout")?;

    let mut head = format!(
        "{method} {} HTTP/1.1\r\nHost: {}:{}\r\nAccept: application/json\r\nConnection: close\r\n",
        target.path, target.host, target.port
    );
    if let Some(body) = body {
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .context("sending request headers")?;
    if let Some(body) = body {
        stream.write_all(body).context("sending request body")?;
    }
    stream.flush().context("flushing request")?;

    read_json(&mut stream)
}

fn read_json(stream: &mut TcpStream) -> Result<Value> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        let n = stream
            .read(&mut chunk)
            .context("reading response headers")?;
        if n == 0 {
            bail!("connection closed before HTTP headers completed");
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = std::str::from_utf8(&buf[..header_end]).context("non-UTF-8 HTTP headers")?;
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or("");
    let code: u16 = status
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .with_context(|| format!("malformed HTTP status line: {status:?}"))?;
    if !(200..300).contains(&code) {
        bail!("HTTP {status}");
    }

    let mut content_length = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("transfer-encoding")
            && value.to_ascii_lowercase().contains("chunked")
        {
            chunked = true;
        }
        if name.eq_ignore_ascii_case("content-length") {
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .with_context(|| format!("bad Content-Length: {value:?}"))?,
            );
        }
    }

    let mut body = buf[header_end + 4..].to_vec();
    match content_length {
        Some(len) => {
            body.truncate(len);
            while body.len() < len {
                let n = stream.read(&mut chunk).context("reading response body")?;
                if n == 0 {
                    bail!("connection closed after {} of {len} body bytes", body.len());
                }
                body.extend_from_slice(&chunk[..n]);
            }
        }
        None if chunked => {
            body = decode_chunked(&mut body, stream)?;
        }
        None => loop {
            let n = stream.read(&mut chunk).context("reading response body")?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        },
    }
    serde_json::from_slice(&body).context("decoding JSON response")
}

/// Decode a chunked body; `prefix` holds bytes already read past the headers.
fn decode_chunked(prefix: &mut Vec<u8>, stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut buf = std::mem::take(prefix);
    let mut body = Vec::new();
    let mut pos = 0;
    let mut chunk = [0u8; 4096];
    loop {
        let size_line = read_line(&mut buf, &mut pos, stream, &mut chunk)?;
        let size_end = size_line
            .iter()
            .position(|&b| b == b';')
            .unwrap_or(size_line.len());
        let size = usize::from_str_radix(
            std::str::from_utf8(&size_line[..size_end])
                .context("non-UTF-8 chunk size")?
                .trim(),
            16,
        )
        .context("bad chunk size")?;
        if size == 0 {
            break;
        }
        while buf.len() - pos < size + 2 {
            let n = stream.read(&mut chunk).context("reading chunk data")?;
            if n == 0 {
                bail!("connection closed inside chunk data");
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        body.extend_from_slice(&buf[pos..pos + size]);
        pos += size + 2; // skip trailing CRLF
    }
    Ok(body)
}

/// Next CRLF-terminated line from the buffer, reading more as needed.
fn read_line(
    buf: &mut Vec<u8>,
    pos: &mut usize,
    stream: &mut TcpStream,
    chunk: &mut [u8; 4096],
) -> Result<Vec<u8>> {
    loop {
        if let Some(i) = buf[*pos..].windows(2).position(|w| w == b"\r\n") {
            let out = buf[*pos..*pos + i].to_vec();
            *pos += i + 2;
            return Ok(out);
        }
        let n = stream.read(chunk).context("reading chunk header")?;
        if n == 0 {
            bail!("connection closed inside chunked body");
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::time::Duration;

    fn serve_once(response: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://127.0.0.1:{}/x",
            listener.local_addr().unwrap().port()
        );
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            stream.write_all(response).unwrap();
        });
        url
    }

    #[test]
    fn decodes_content_length_and_chunked() {
        let plain = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\n{\"a\":1}",
        );
        assert_eq!(
            get_json(&plain, Duration::from_secs(5)).unwrap(),
            serde_json::json!({"a": 1})
        );
        let chunked = serve_once(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n7\r\n{\"a\":1}\r\n0\r\n\r\n",
        );
        assert_eq!(
            get_json(&chunked, Duration::from_secs(5)).unwrap(),
            serde_json::json!({"a": 1})
        );
    }
}
