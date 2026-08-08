//! Talk to a local Ollama server's embedding endpoint. This is the only code
//! in subrosa that opens a socket, and only `subrosa embed` /
//! `search --semantic` reach it — recall, the hooks and ingest never do.
//!
//! ponytail: a fixed-endpoint localhost client, not an HTTP library. One POST,
//! plain HTTP, no redirects, no keep-alive, no TLS. Anything more (a remote
//! host, auth, streaming) needs a real HTTP crate, which the 7-crate budget
//! says no to.

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use serde_json::Value;

/// Hard ceiling on one reply. A 64-turn batch at 4096 dimensions is under
/// 2 MiB, so this only ever stops a runaway or misconfigured peer from growing
/// the buffer until the process is killed.
const MAX_RESPONSE: usize = 64 << 20;

/// Embed a batch of texts in one `POST /api/embed`. One vector per input, in order.
pub fn embed(
    host: &str,
    model: &str,
    inputs: &[String],
    connect_timeout: Duration,
    io_timeout: Duration,
) -> Result<Vec<Vec<f32>>, String> {
    let addr = host_addr(host)?;
    let raw = roundtrip(
        &addr,
        &build_request(&addr, model, inputs),
        connect_timeout,
        io_timeout,
        MAX_RESPONSE,
    )?;
    let (head, body) = split_response(&raw)?;
    check_status(&head, body, model)?;
    parse_embeddings(body)
}

/// Cosine similarity of two same-length, L2-normalized vectors — a plain dot
/// product. Accumulated in f64: an f32 accumulator overflows to infinity on a
/// corrupt vector with huge components, and infinity outranks every real score.
/// Callers check the dimensions and that the result is in range; nothing relies
/// on this to catch either.
pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    a.iter().zip(b).map(|(x, y)| *x as f64 * *y as f64).sum()
}

/// L2-normalize in place so ranking is a dot product. A zero vector stays zero.
pub fn normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// `host` as a `host:port` string, with `http://` tolerated. TLS is rejected on
/// purpose: this only ever talks to a model server on your own machine.
fn host_addr(host: &str) -> Result<String, String> {
    let h = host.trim().trim_end_matches('/');
    if h.starts_with("https://") {
        return Err(format!(
            "{h}: subrosa talks plain HTTP to a local Ollama — drop the https://"
        ));
    }
    let h = h.strip_prefix("http://").unwrap_or(h);
    if h.is_empty() {
        return Err("no Ollama host configured (SUBROSA_OLLAMA_HOST)".to_string());
    }
    Ok(if h.contains(':') {
        h.to_string()
    } else {
        format!("{h}:11434")
    })
}

/// HTTP/1.0 plus `Connection: close` means the server answers close-delimited
/// and never chunked, so reading to EOF is the whole body.
fn build_request(host: &str, model: &str, inputs: &[String]) -> Vec<u8> {
    let body = serde_json::json!({ "model": model, "input": inputs }).to_string();
    let mut req = format!(
        "POST /api/embed HTTP/1.0\r\n\
         Host: {host}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    req.extend_from_slice(body.as_bytes());
    req
}

fn roundtrip(
    addr: &str,
    req: &[u8],
    connect_timeout: Duration,
    io_timeout: Duration,
    max_bytes: usize,
) -> Result<Vec<u8>, String> {
    let down = || format!("cannot reach Ollama at {addr} — is it running? (ollama serve)");
    let stalled = || format!("Ollama at {addr} stopped answering — gave up after {io_timeout:?}");
    // Every resolved address, not just the first: `localhost` resolves to ::1
    // ahead of 127.0.0.1 on macOS, and Ollama binds 127.0.0.1 by default.
    let mut stream = addr
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve Ollama host {addr}: {e}"))?
        .find_map(|sock| TcpStream::connect_timeout(&sock, connect_timeout).ok())
        .ok_or_else(down)?;
    stream
        .set_write_timeout(Some(io_timeout))
        .map_err(|e| format!("cannot set the Ollama socket timeouts: {e}"))?;
    let deadline = Instant::now() + io_timeout;
    stream
        .write_all(req)
        .and_then(|_| stream.flush())
        .map_err(|_| down())?;

    // Bounded read: a socket timeout only limits ONE read, so a peer that drips
    // a byte at a time could hold us forever, and an unbounded buffer could grow
    // until the allocator gives up (fatal under panic=abort). Cap both.
    let mut raw = Vec::new();
    let mut chunk = [0u8; 16 << 10];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(stalled());
        }
        stream
            .set_read_timeout(Some(left))
            .map_err(|e| format!("cannot set the Ollama read timeout: {e}"))?;
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(raw),
            Ok(n) if raw.len() + n > max_bytes => {
                return Err(format!(
                    "Ollama at {addr} sent more than {max_bytes} bytes — refusing to read on"
                ))
            }
            Ok(n) => raw.extend_from_slice(&chunk[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return Err(stalled())
            }
            Err(e) => return Err(format!("Ollama at {addr} did not answer: {e}")),
        }
    }
}

/// Split a response at the blank line between headers and body.
fn split_response(raw: &[u8]) -> Result<(String, &[u8]), String> {
    let sep = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| format!("Ollama sent a malformed reply: {}", snippet(raw)))?;
    Ok((
        String::from_utf8_lossy(&raw[..sep]).into_owned(),
        &raw[sep + 4..],
    ))
}

/// Turn a non-200 into something actionable. Ollama answers a model it never
/// pulled with 404 and an `{"error": ...}` body.
fn check_status(head: &str, body: &[u8], model: &str) -> Result<(), String> {
    let status = head.lines().next().unwrap_or_default();
    let code: u16 = status
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| format!("Ollama sent a malformed status line: {status}"))?;
    if code == 200 {
        return Ok(());
    }
    if code == 404 {
        return Err(format!(
            "model '{model}' not found — run: ollama pull {model}"
        ));
    }
    let detail = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_string))
        .unwrap_or_else(|| snippet(body));
    Err(format!("Ollama returned HTTP {code}: {detail}"))
}

fn parse_embeddings(body: &[u8]) -> Result<Vec<Vec<f32>>, String> {
    let v: Value = serde_json::from_slice(body)
        .map_err(|e| format!("Ollama sent an unreadable reply ({e}): {}", snippet(body)))?;
    let rows = v["embeddings"]
        .as_array()
        .ok_or_else(|| format!("Ollama reply carries no embeddings: {}", snippet(body)))?;
    // Anything that isn't a full row of finite numbers means the model gave back
    // something unusable. Coercing it to zeros would cache a vector that scores
    // against everything and can never be told apart from a real one afterwards.
    rows.iter()
        .map(|row| {
            let xs = row
                .as_array()
                .ok_or_else(|| "Ollama reply holds a non-array embedding".to_string())?;
            if xs.is_empty() {
                return Err("Ollama returned an empty embedding".to_string());
            }
            xs.iter()
                .map(|x| {
                    x.as_f64()
                        .map(|f| f as f32)
                        .filter(|f| f.is_finite())
                        .ok_or_else(|| format!("Ollama returned an unusable embedding value: {x}"))
                })
                .collect()
        })
        .collect()
}

/// The head of an unexpected reply, so the error says what actually came back.
fn snippet(b: &[u8]) -> String {
    String::from_utf8_lossy(&b[..b.len().min(120)])
        .replace(['\r', '\n'], " ")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_scores_normalized_vectors() {
        let a = [1.0, 0.0, 0.0];
        assert_eq!(cosine(&a, &a), 1.0);
        assert_eq!(cosine(&a, &[0.0, 1.0, 0.0]), 0.0);
        assert_eq!(cosine(&a, &[-1.0, 0.0, 0.0]), -1.0);
        // Different dimensions score over the shared prefix, never panic.
        assert_eq!(cosine(&a, &[1.0, 0.0]), 1.0);
    }

    #[test]
    fn normalize_makes_a_unit_vector() {
        let mut v = [3.0, 4.0];
        normalize(&mut v);
        assert_eq!(v, [0.6, 0.8]);
        let mut zero = [0.0, 0.0];
        normalize(&mut zero);
        assert_eq!(zero, [0.0, 0.0]);
    }

    #[test]
    fn host_addr_defaults_the_port_and_refuses_tls() {
        assert_eq!(host_addr("localhost:11434").unwrap(), "localhost:11434");
        assert_eq!(
            host_addr("http://127.0.0.1:1234/").unwrap(),
            "127.0.0.1:1234"
        );
        assert_eq!(host_addr("localhost").unwrap(), "localhost:11434");
        assert!(host_addr("https://ollama.example.com").is_err());
        assert!(host_addr("  ").is_err());
    }

    #[test]
    fn build_request_sends_a_batched_json_body() {
        let req = build_request(
            "localhost:11434",
            "nomic-embed-text",
            &["one".to_string(), "two".to_string()],
        );
        let text = String::from_utf8(req).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("POST /api/embed HTTP/1.0\r\n"), "{head}");
        assert!(head.contains("Host: localhost:11434"));
        assert!(head.contains("Connection: close"));
        assert!(head.contains(&format!("Content-Length: {}", body.len())));
        let v: Value = serde_json::from_str(body).unwrap();
        assert_eq!(v["model"], "nomic-embed-text");
        assert_eq!(v["input"], serde_json::json!(["one", "two"]));
    }

    #[test]
    fn split_response_cuts_at_the_blank_line() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"a\":1}";
        let (head, body) = split_response(raw).unwrap();
        assert!(head.starts_with("HTTP/1.1 200 OK"));
        assert_eq!(body, b"{\"a\":1}");
        assert!(split_response(b"garbage with no header break").is_err());
    }

    #[test]
    fn parse_embeddings_reads_the_batched_shape() {
        let got = parse_embeddings(br#"{"embeddings":[[0.5,-0.5],[1.0,0.0]]}"#).unwrap();
        assert_eq!(got, vec![vec![0.5, -0.5], vec![1.0, 0.0]]);
        assert!(parse_embeddings(b"not json").is_err());
        // The superseded /api/embeddings shape has no "embeddings" array.
        assert!(parse_embeddings(br#"{"embedding":[0.1]}"#).is_err());
    }

    /// A vector we can't fully trust is an error, never a fabricated zero row.
    #[test]
    fn parse_embeddings_rejects_unusable_vectors() {
        for bad in [
            r#"{"embeddings":[[0.5,"oops"]]}"#,
            r#"{"embeddings":[[0.5,null]]}"#,
            r#"{"embeddings":[[]]}"#,
            r#"{"embeddings":[[0.1],"nope"]}"#,
            // JSON has no NaN/Infinity literal, so an overflowing number is how a
            // non-finite value actually arrives. 1e400 is already infinite as an
            // f64; 1e39 is a perfectly good f64 that only overflows on the cast
            // to f32 — the case a check made before the cast would miss.
            r#"{"embeddings":[[1e400]]}"#,
            r#"{"embeddings":[[1e39]]}"#,
        ] {
            assert!(parse_embeddings(bad.as_bytes()).is_err(), "accepted {bad}");
        }
        // The same magnitude one exponent down still fits an f32 and is kept.
        assert!(parse_embeddings(br#"{"embeddings":[[1e38]]}"#).is_ok());
    }

    /// Serve one canned reply on loopback, then hand back the port.
    fn listener(reply: Option<Vec<u8>>) -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = l.accept() {
                let mut sink = [0u8; 4096];
                let _ = sock.read(&mut sink);
                match reply {
                    Some(bytes) => {
                        let _ = sock.write_all(&bytes);
                    }
                    // Hold the connection open and never answer — the slow drip.
                    None => std::thread::sleep(Duration::from_secs(30)),
                }
            }
        });
        addr
    }

    #[test]
    fn roundtrip_refuses_a_reply_bigger_than_the_cap() {
        let mut reply = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        reply.extend(std::iter::repeat_n(b'x', 8192));
        let addr = listener(Some(reply));
        let err = roundtrip(
            &addr,
            b"GET / HTTP/1.0\r\n\r\n",
            Duration::from_secs(2),
            Duration::from_secs(5),
            512,
        )
        .unwrap_err();
        assert!(err.contains("more than 512 bytes"), "{err}");
    }

    #[test]
    fn roundtrip_gives_up_on_a_peer_that_never_answers() {
        let addr = listener(None);
        let started = Instant::now();
        let err = roundtrip(
            &addr,
            b"GET / HTTP/1.0\r\n\r\n",
            Duration::from_secs(2),
            Duration::from_millis(200),
            1 << 20,
        )
        .unwrap_err();
        assert!(err.contains("stopped answering"), "{err}");
        // The deadline is absolute, so this can't sit there for the peer's 30s.
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "waited too long"
        );
    }

    #[test]
    fn check_status_names_the_pull_command_on_404() {
        let ok = "HTTP/1.1 200 OK";
        assert!(check_status(ok, b"", "m").is_ok());
        let missing = check_status(
            "HTTP/1.1 404 Not Found",
            br#"{"error":"model \"m\" not found"}"#,
            "m",
        )
        .unwrap_err();
        assert_eq!(missing, "model 'm' not found — run: ollama pull m");
        // Any other failure surfaces Ollama's own error text.
        let boom =
            check_status("HTTP/1.1 500 Oops", br#"{"error":"out of memory"}"#, "m").unwrap_err();
        assert_eq!(boom, "Ollama returned HTTP 500: out of memory");
    }
}
