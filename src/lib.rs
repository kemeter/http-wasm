//! A Rust host for the [http-wasm](https://http-wasm.io) handler ABI.
//!
//! http-wasm is a portable ABI for HTTP middleware compiled to WebAssembly: a
//! guest exports `handle_request`/`handle_response` and the host exposes the
//! current request/response through a small set of functions. This crate is the
//! **host** side — it loads a guest `.wasm` and runs it against whatever HTTP
//! state you provide by implementing the [`Host`] trait.
//!
//! It is deliberately library-agnostic: it knows nothing about any specific
//! HTTP stack, so the same guest runs on any embedder.
//!
//! # Example
//!
//! ```no_run
//! use http_wasm_host::{Plugin, Limits, Next};
//! # fn load() -> Vec<u8> { Vec::new() }
//! # struct MyHost;
//! # impl http_wasm_host::Host for MyHost {
//! #     fn method(&self) -> String { "GET".into() }
//! #     fn set_method(&mut self, _: &str) {}
//! #     fn uri(&self) -> String { "/".into() }
//! #     fn set_uri(&mut self, _: &str) {}
//! #     fn protocol_version(&self) -> String { "HTTP/1.1".into() }
//! #     fn source_addr(&self) -> String { String::new() }
//! #     fn status_code(&self) -> u32 { 200 }
//! #     fn set_status_code(&mut self, _: u32) {}
//! #     fn header_names(&self, _: http_wasm_host::HeaderKind) -> Vec<String> { vec![] }
//! #     fn header_values(&self, _: http_wasm_host::HeaderKind, _: &str) -> Vec<String> { vec![] }
//! #     fn set_header_value(&mut self, _: http_wasm_host::HeaderKind, _: &str, _: &str) {}
//! #     fn add_header_value(&mut self, _: http_wasm_host::HeaderKind, _: &str, _: &str) {}
//! #     fn remove_header(&mut self, _: http_wasm_host::HeaderKind, _: &str) {}
//! #     fn read_body(&mut self, _: http_wasm_host::HeaderKind, _: usize) -> Vec<u8> { vec![] }
//! #     fn write_body(&mut self, _: http_wasm_host::HeaderKind, _: &[u8]) {}
//! # }
//! let plugin = Plugin::from_bytes(&load(), Limits::default())?;
//! let mut host = MyHost;
//! match plugin.handle_request(&mut host)? {
//!     Next::Continue(ctx) => {
//!         // forward to the backend, then:
//!         plugin.handle_response(&mut host, ctx, false)?;
//!     }
//!     Next::Stop => { /* guest wrote the response itself */ }
//! }
//! # Ok::<(), http_wasm_host::Error>(())
//! ```

mod abi;
mod host;
mod runtime;

pub use abi::{HeaderKind, LogLevel, features};
pub use host::Host;
pub use runtime::{Error, Limits, Next, Plugin};
