//! Runtime behaviour independent of any fixture crate: instance lifecycle and
//! the per-call timeout. The guest is inline WAT; it counts its calls in a global
//! (reported as the status code) and picks a behaviour from the request method:
//! `SPIN` loops forever, `WORK` burns a few milliseconds, `TRAP` traps after
//! counting, anything else returns at once.

use std::time::{Duration, Instant};

use http_wasm_host::{Error, HeaderKind, Host, Limits, Next, Plugin};

const GUEST: &str = r#"
(module
  (import "http_handler" "get_method" (func $get_method (param i32 i32) (result i32)))
  (import "http_handler" "set_status_code" (func $set_status (param i32)))
  (memory (export "memory") 1)
  (global $calls (mut i32) (i32.const 0))
  (func (export "handle_request") (result i64)
    (local $i i32)
    (global.set $calls (i32.add (global.get $calls) (i32.const 1)))
    (call $set_status (global.get $calls))
    (drop (call $get_method (i32.const 0) (i32.const 16)))
    ;; SPIN: loop forever
    (if (i32.eq (i32.load8_u (i32.const 0)) (i32.const 83))
      (then (loop $forever (br $forever))))
    ;; WORK: a bounded busy loop
    (if (i32.eq (i32.load8_u (i32.const 0)) (i32.const 87))
      (then
        (local.set $i (i32.const 5000000))
        (loop $work
          (local.set $i (i32.sub (local.get $i) (i32.const 1)))
          (br_if $work (i32.gt_s (local.get $i) (i32.const 0))))))
    ;; TRAP
    (if (i32.eq (i32.load8_u (i32.const 0)) (i32.const 84))
      (then unreachable))
    (i64.const 1))
  (func (export "handle_response") (param i32 i32)))
"#;

struct TestHost {
    method: String,
    status: u32,
}

impl TestHost {
    fn new(method: &str) -> Self {
        Self {
            method: method.into(),
            status: 0,
        }
    }
}

impl Host for TestHost {
    fn method(&self) -> String {
        self.method.clone()
    }
    fn set_method(&mut self, m: &str) {
        self.method = m.into();
    }
    fn uri(&self) -> String {
        "/".into()
    }
    fn set_uri(&mut self, _: &str) {}
    fn protocol_version(&self) -> String {
        "HTTP/1.1".into()
    }
    fn source_addr(&self) -> String {
        String::new()
    }
    fn status_code(&self) -> u32 {
        self.status
    }
    fn set_status_code(&mut self, s: u32) {
        self.status = s;
    }
    fn header_names(&self, _: HeaderKind) -> Vec<String> {
        vec![]
    }
    fn header_values(&self, _: HeaderKind, _: &str) -> Vec<String> {
        vec![]
    }
    fn set_header_value(&mut self, _: HeaderKind, _: &str, _: &str) {}
    fn add_header_value(&mut self, _: HeaderKind, _: &str, _: &str) {}
    fn remove_header(&mut self, _: HeaderKind, _: &str) {}
    fn read_body(&mut self, _: HeaderKind, _: usize) -> Vec<u8> {
        vec![]
    }
    fn write_body(&mut self, _: HeaderKind, _: &[u8]) {}
}

fn plugin(limits: Limits) -> Plugin {
    Plugin::from_bytes(GUEST.as_bytes(), limits).unwrap()
}

/// Run one request and return the guest's call count (its status code).
fn calls_seen(plugin: &Plugin, method: &str) -> Result<u32, Error> {
    let mut host = TestHost::new(method);
    let next = plugin.handle_request(&mut host)?;
    assert_eq!(next, Next::Continue(0));
    Ok(host.status)
}

#[test]
fn fresh_instance_per_call_by_default() {
    let plugin = plugin(Limits::default());
    assert_eq!(calls_seen(&plugin, "GET").unwrap(), 1);
    assert_eq!(calls_seen(&plugin, "GET").unwrap(), 1);
}

#[test]
fn runaway_call_times_out() {
    let plugin = plugin(Limits {
        timeout: Duration::from_millis(50),
        ..Limits::default()
    });
    let start = Instant::now();
    assert!(matches!(calls_seen(&plugin, "SPIN"), Err(Error::Run(_))));
    assert!(start.elapsed() < Duration::from_secs(2));
    // The plugin keeps serving afterwards.
    assert_eq!(calls_seen(&plugin, "GET").unwrap(), 1);
}

#[test]
fn slow_call_timing_out_does_not_trip_concurrent_calls() {
    // One call spins until its timeout while another thread keeps running
    // short calls on the same plugin across that moment. Only the spinning call
    // may fail: the timeout must be per call, not a shared epoch bump that also
    // interrupts whatever else is running.
    let plugin = plugin(Limits {
        timeout: Duration::from_millis(200),
        ..Limits::default()
    });
    std::thread::scope(|scope| {
        let spinner = scope.spawn(|| calls_seen(&plugin, "SPIN"));
        let until = Instant::now() + Duration::from_millis(500);
        let mut runs = 0;
        while Instant::now() < until {
            calls_seen(&plugin, "WORK").expect("a concurrent call was interrupted");
            runs += 1;
        }
        assert!(runs > 1);
        assert!(matches!(spinner.join().unwrap(), Err(Error::Run(_))));
    });
}
