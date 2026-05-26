//! Optional outbound-HTTP extension to the http-wasm ABI.
//!
//! The base http-wasm spec has no way for a guest to make a network call. Some
//! middleware needs it — a CrowdSec bouncer querying its LAPI, an auth plugin
//! calling a token endpoint. This module adds one extra host function,
//! `http_fetch`, **outside** the standard ABI. A guest that uses it is no
//! longer portable to a vanilla http-wasm host; that trade-off is the
//! embedder's to make, so the function is only wired in when a [`Fetcher`] is
//! provided (see [`Plugin::with_fetcher`](crate::Plugin::with_fetcher)).
//!
//! The host crate stays runtime-agnostic: it never does I/O itself. The
//! embedder implements [`Fetcher`] (typically bridging to its own async HTTP
//! client) and is responsible for any sandboxing — e.g. validating the URL
//! against an allow-list before performing the request.

/// A request the guest asked the host to perform.
#[derive(Debug, Clone)]
pub struct FetchRequest {
    /// HTTP method, e.g. `"GET"`.
    pub method: String,
    /// Full target URL, e.g. `"http://crowdsec:8080/v1/decisions?ip=1.2.3.4"`.
    pub url: String,
    /// Request headers as `(name, value)` pairs.
    pub headers: Vec<(String, String)>,
    /// Request body (may be empty).
    pub body: Vec<u8>,
}

/// The host's reply to a [`FetchRequest`].
#[derive(Debug, Clone)]
pub struct FetchResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Performs outbound HTTP on the guest's behalf. Implemented by the embedder.
///
/// Implementations decide the policy: which URLs are allowed (anti-SSRF),
/// timeouts, TLS, redirects. Returning `Err` denies the call; the guest sees a
/// failure result. This runs synchronously inside a guest call, so an async
/// embedder typically blocks on its runtime here.
pub trait Fetcher: Send + Sync {
    fn fetch(&self, request: FetchRequest) -> Result<FetchResponse, String>;
}

/// Outcome of a fire-and-forget [`Sink::send`] enqueue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// The request was accepted into the queue.
    Queued,
    /// The queue is full; the request was dropped.
    QueueFull,
    /// The request was rejected (e.g. host not allowed).
    Rejected,
}

/// Accepts a request to perform later, without blocking the guest. Implemented
/// by the embedder (typically pushing onto a bounded queue drained by a
/// background worker). Unlike [`Fetcher`], the guest never sees a response —
/// this is fire-and-forget, for things like analytics beacons where adding the
/// network round-trip to the request path would be wrong.
///
/// The embedder owns the policy here too: queue size, batching, and URL
/// allow-listing (anti-SSRF).
pub trait Sink: Send + Sync {
    fn send(&self, request: FetchRequest) -> SendOutcome;
}

/// Wire format for `http_fetch`, kept dependency-free (no serde): a sequence of
/// length-prefixed fields. All lengths are little-endian `u32`.
///
/// Request  = method, url, header-count, [name, value]*, body
/// Response = status(u32), header-count, [name, value]*, body
pub(crate) mod codec {
    use super::{FetchRequest, FetchResponse};

    fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
        out.extend_from_slice(&(b.len() as u32).to_le_bytes());
        out.extend_from_slice(b);
    }

    fn take_bytes(buf: &[u8], pos: &mut usize) -> Option<Vec<u8>> {
        let len_end = pos.checked_add(4)?;
        if len_end > buf.len() {
            return None;
        }
        let len = u32::from_le_bytes(buf[*pos..len_end].try_into().ok()?) as usize;
        let data_end = len_end.checked_add(len)?;
        if data_end > buf.len() {
            return None;
        }
        let out = buf[len_end..data_end].to_vec();
        *pos = data_end;
        Some(out)
    }

    fn take_u32(buf: &[u8], pos: &mut usize) -> Option<u32> {
        let end = pos.checked_add(4)?;
        if end > buf.len() {
            return None;
        }
        let v = u32::from_le_bytes(buf[*pos..end].try_into().ok()?);
        *pos = end;
        Some(v)
    }

    /// Decode a request the guest wrote into linear memory.
    pub(crate) fn decode_request(buf: &[u8]) -> Option<FetchRequest> {
        let mut pos = 0;
        let method = String::from_utf8(take_bytes(buf, &mut pos)?).ok()?;
        let url = String::from_utf8(take_bytes(buf, &mut pos)?).ok()?;
        let count = take_u32(buf, &mut pos)?;
        let mut headers = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let name = String::from_utf8(take_bytes(buf, &mut pos)?).ok()?;
            let value = String::from_utf8(take_bytes(buf, &mut pos)?).ok()?;
            headers.push((name, value));
        }
        let body = take_bytes(buf, &mut pos)?;
        Some(FetchRequest {
            method,
            url,
            headers,
            body,
        })
    }

    /// Encode a response for the guest to read back.
    pub(crate) fn encode_response(resp: &FetchResponse) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(resp.status as u32).to_le_bytes());
        out.extend_from_slice(&(resp.headers.len() as u32).to_le_bytes());
        for (name, value) in &resp.headers {
            put_bytes(&mut out, name.as_bytes());
            put_bytes(&mut out, value.as_bytes());
        }
        put_bytes(&mut out, &resp.body);
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn request_roundtrips() {
            // Build a request the way the guest would, then decode it.
            let mut buf = Vec::new();
            put_bytes(&mut buf, b"POST");
            put_bytes(&mut buf, b"http://x/y");
            buf.extend_from_slice(&1u32.to_le_bytes());
            put_bytes(&mut buf, b"x-api-key");
            put_bytes(&mut buf, b"secret");
            put_bytes(&mut buf, b"hello");

            let req = decode_request(&buf).unwrap();
            assert_eq!(req.method, "POST");
            assert_eq!(req.url, "http://x/y");
            assert_eq!(req.headers, vec![("x-api-key".into(), "secret".into())]);
            assert_eq!(req.body, b"hello");
        }

        #[test]
        fn truncated_request_is_rejected() {
            assert!(decode_request(&[0, 0]).is_none());
        }

        #[test]
        fn response_encodes_status_and_body() {
            let resp = FetchResponse {
                status: 403,
                headers: vec![],
                body: b"banned".to_vec(),
            };
            let enc = encode_response(&resp);
            assert_eq!(u32::from_le_bytes(enc[0..4].try_into().unwrap()), 403);
        }
    }
}
