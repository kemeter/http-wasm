//! End-to-end test: compile the fixture guest to wasm, load it through the
//! host, and assert the ABI round-trips (header read, header add, status,
//! short-circuit).

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use http_wasm_host::{HeaderKind, Host, Limits, Next, Plugin};

/// A self-contained in-memory request/response used to drive a guest.
#[derive(Default)]
struct TestHost {
    method: String,
    uri: String,
    status: u32,
    req_headers: HashMap<String, Vec<String>>,
    resp_headers: HashMap<String, Vec<String>>,
    resp_body: Vec<u8>,
}

impl TestHost {
    fn with_req_header(mut self, name: &str, value: &str) -> Self {
        self.req_headers
            .entry(name.to_ascii_lowercase())
            .or_default()
            .push(value.to_string());
        self
    }
    fn map(&mut self, kind: HeaderKind) -> &mut HashMap<String, Vec<String>> {
        match kind {
            HeaderKind::Request | HeaderKind::RequestTrailers => &mut self.req_headers,
            HeaderKind::Response | HeaderKind::ResponseTrailers => &mut self.resp_headers,
        }
    }
}

impl Host for TestHost {
    fn method(&self) -> String {
        self.method.clone()
    }
    fn set_method(&mut self, m: &str) {
        self.method = m.to_string();
    }
    fn uri(&self) -> String {
        self.uri.clone()
    }
    fn set_uri(&mut self, u: &str) {
        self.uri = u.to_string();
    }
    fn protocol_version(&self) -> String {
        "HTTP/1.1".to_string()
    }
    fn source_addr(&self) -> String {
        "127.0.0.1:1234".to_string()
    }
    fn status_code(&self) -> u32 {
        self.status
    }
    fn set_status_code(&mut self, s: u32) {
        self.status = s;
    }
    fn header_names(&self, kind: HeaderKind) -> Vec<String> {
        let m = match kind {
            HeaderKind::Request | HeaderKind::RequestTrailers => &self.req_headers,
            HeaderKind::Response | HeaderKind::ResponseTrailers => &self.resp_headers,
        };
        m.keys().cloned().collect()
    }
    fn header_values(&self, kind: HeaderKind, name: &str) -> Vec<String> {
        let m = match kind {
            HeaderKind::Request | HeaderKind::RequestTrailers => &self.req_headers,
            HeaderKind::Response | HeaderKind::ResponseTrailers => &self.resp_headers,
        };
        m.get(&name.to_ascii_lowercase())
            .cloned()
            .unwrap_or_default()
    }
    fn set_header_value(&mut self, kind: HeaderKind, name: &str, value: &str) {
        self.map(kind)
            .insert(name.to_ascii_lowercase(), vec![value.to_string()]);
    }
    fn add_header_value(&mut self, kind: HeaderKind, name: &str, value: &str) {
        self.map(kind)
            .entry(name.to_ascii_lowercase())
            .or_default()
            .push(value.to_string());
    }
    fn remove_header(&mut self, kind: HeaderKind, name: &str) {
        self.map(kind).remove(&name.to_ascii_lowercase());
    }
    fn read_body(&mut self, _kind: HeaderKind, _max: usize) -> Vec<u8> {
        Vec::new()
    }
    fn write_body(&mut self, kind: HeaderKind, data: &[u8]) {
        if matches!(kind, HeaderKind::Response) {
            self.resp_body = data.to_vec();
        }
    }
}

/// Compile a fixture guest and return its wasm bytes. `fixture` is the
/// directory name under tests/fixtures and `lib` the produced module file stem.
fn build_guest(fixture: &str, lib: &str) -> Vec<u8> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture);
    let status = Command::new("cargo")
        .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
        .current_dir(&dir)
        .status()
        .expect("cargo build for guest");
    assert!(status.success(), "guest build failed");
    let wasm = dir.join(format!("target/wasm32-unknown-unknown/release/{lib}.wasm"));
    std::fs::read(&wasm).expect("read compiled guest wasm")
}

fn guest_wasm() -> &'static [u8] {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    WASM.get_or_init(|| build_guest("guest-headers", "guest_headers"))
}

fn fetch_guest_wasm() -> &'static [u8] {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    WASM.get_or_init(|| build_guest("guest-fetch", "guest_fetch"))
}

fn send_guest_wasm() -> &'static [u8] {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    WASM.get_or_init(|| build_guest("guest-send", "guest_send"))
}

/// Records every request the guest fires, so the test can inspect it.
#[derive(Default)]
struct RecordingSink {
    sent: std::sync::Mutex<Vec<http_wasm_host::FetchRequest>>,
}

impl http_wasm_host::Sink for RecordingSink {
    fn send(&self, request: http_wasm_host::FetchRequest) -> http_wasm_host::SendOutcome {
        self.sent.lock().unwrap().push(request);
        http_wasm_host::SendOutcome::Queued
    }
}

/// A canned fetcher returning a fixed status, recording the request it saw.
struct CannedFetcher {
    status: u16,
}

impl http_wasm_host::Fetcher for CannedFetcher {
    fn fetch(
        &self,
        _request: http_wasm_host::FetchRequest,
    ) -> Result<http_wasm_host::FetchResponse, String> {
        Ok(http_wasm_host::FetchResponse {
            status: self.status,
            headers: vec![],
            body: Vec::new(),
        })
    }
}

#[test]
fn guest_continues_and_adds_response_header() {
    let plugin = Plugin::from_bytes(guest_wasm(), Limits::default()).unwrap();
    let mut host = TestHost {
        method: "GET".into(),
        uri: "/".into(),
        status: 200,
        ..Default::default()
    };

    let next = plugin.handle_request(&mut host).unwrap();
    assert_eq!(next, Next::Continue(0));
    assert_eq!(
        host.header_values(HeaderKind::Response, "x-greeted"),
        vec!["true"]
    );
}

#[test]
fn guest_short_circuits_when_blocked() {
    let plugin = Plugin::from_bytes(guest_wasm(), Limits::default()).unwrap();
    let mut host = TestHost {
        method: "GET".into(),
        uri: "/".into(),
        status: 200,
        ..Default::default()
    }
    .with_req_header("x-block", "yes");

    let next = plugin.handle_request(&mut host).unwrap();
    assert_eq!(next, Next::Stop);
    assert_eq!(host.status_code(), 403);
    assert_eq!(host.resp_body, b"blocked by guest");
}

#[test]
fn fetch_guest_blocks_when_decision_api_denies() {
    use std::sync::Arc;
    let plugin = Plugin::from_bytes(fetch_guest_wasm(), Limits::default())
        .unwrap()
        .with_fetcher(Arc::new(CannedFetcher { status: 403 }));
    let mut host = TestHost {
        method: "GET".into(),
        uri: "/".into(),
        status: 200,
        ..Default::default()
    };

    let next = plugin.handle_request(&mut host).unwrap();
    assert_eq!(next, Next::Stop);
    assert_eq!(host.status_code(), 403);
    assert_eq!(host.resp_body, b"denied by decision api");
}

#[test]
fn fetch_guest_allows_when_decision_api_permits() {
    use std::sync::Arc;
    let plugin = Plugin::from_bytes(fetch_guest_wasm(), Limits::default())
        .unwrap()
        .with_fetcher(Arc::new(CannedFetcher { status: 200 }));
    let mut host = TestHost {
        method: "GET".into(),
        uri: "/".into(),
        status: 200,
        ..Default::default()
    };

    let next = plugin.handle_request(&mut host).unwrap();
    assert_eq!(next, Next::Continue(0));
}

#[test]
fn send_guest_fires_beacon_and_continues() {
    use std::sync::Arc;
    let sink = Arc::new(RecordingSink::default());
    let plugin = Plugin::from_bytes(send_guest_wasm(), Limits::default())
        .unwrap()
        .with_sink(sink.clone());
    let mut host = TestHost {
        method: "GET".into(),
        uri: "/".into(),
        status: 200,
        ..Default::default()
    };

    // Guest continues regardless (fire-and-forget).
    let next = plugin.handle_request(&mut host).unwrap();
    assert_eq!(next, Next::Continue(0));

    // …and the beacon reached the sink with the expected shape.
    let sent = sink.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].method, "POST");
    assert_eq!(sent[0].url, "http://collector/api/send");
    assert_eq!(sent[0].body, br#"{"type":"event"}"#);
}
