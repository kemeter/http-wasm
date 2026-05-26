//! The neutral host contract.
//!
//! An embedder (a proxy, a server, …) implements [`Host`] to expose the current
//! HTTP request/response to a guest plugin. This trait is the entire surface a
//! guest can touch — it deliberately knows nothing about any specific HTTP
//! library, so the same guest runs unchanged on any embedder.

use crate::abi::{HeaderKind, LogLevel};

/// Per-request state the embedder exposes to a running guest.
///
/// All methods operate on the request *or* response depending on `kind`. The
/// host owns the data; the guest reads and mutates it through these calls.
/// Methods take `&mut self` because guests routinely mutate headers, status and
/// body during the request and response phases.
pub trait Host {
    // --- Request line ---

    /// HTTP method, e.g. `"GET"`.
    fn method(&self) -> String;
    /// Replace the HTTP method.
    fn set_method(&mut self, method: &str);

    /// Request URI including path and query, e.g. `"/a?b=c"`.
    fn uri(&self) -> String;
    /// Replace the request URI.
    fn set_uri(&mut self, uri: &str);

    /// Protocol version string, e.g. `"HTTP/1.1"`.
    fn protocol_version(&self) -> String;

    /// Client address as `ip:port`, or empty if unknown.
    fn source_addr(&self) -> String;

    // --- Status ---

    /// Current response status code. Meaningful in the response phase.
    fn status_code(&self) -> u32;
    /// Set the response status code.
    fn set_status_code(&mut self, status: u32);

    // --- Headers (and trailers) ---

    /// All header names for `kind`, lowercased. Order is not significant.
    fn header_names(&self, kind: HeaderKind) -> Vec<String>;
    /// All values for the named header under `kind` (case-insensitive name).
    fn header_values(&self, kind: HeaderKind, name: &str) -> Vec<String>;
    /// Replace every value of `name` with the single `value`.
    fn set_header_value(&mut self, kind: HeaderKind, name: &str, value: &str);
    /// Append `value` to `name` without removing existing values.
    fn add_header_value(&mut self, kind: HeaderKind, name: &str, value: &str);
    /// Remove every value of `name`.
    fn remove_header(&mut self, kind: HeaderKind, name: &str);

    // --- Body ---

    /// Read up to `max` bytes of the `kind` body, advancing an internal cursor.
    /// Returns the bytes read; an empty slice means EOF. The runtime turns this
    /// into the ABI's `(eof, len)` result.
    fn read_body(&mut self, kind: HeaderKind, max: usize) -> Vec<u8>;
    /// Overwrite the `kind` body with `data` (or append on later calls — the
    /// embedder decides; the first call replaces).
    fn write_body(&mut self, kind: HeaderKind, data: &[u8]);

    // --- Administrative ---

    /// Guest-specific configuration bytes (often JSON), or empty if none.
    fn config(&self) -> Vec<u8> {
        Vec::new()
    }

    /// Emit a log line. Default forwards to the `log` crate.
    fn log(&self, level: LogLevel, message: &str) {
        match level {
            LogLevel::Debug => log::debug!("{message}"),
            LogLevel::Info => log::info!("{message}"),
            LogLevel::Warn => log::warn!("{message}"),
            LogLevel::Error => log::error!("{message}"),
            LogLevel::None => {}
        }
    }

    /// Whether `level` would be emitted. Default: everything except `None`.
    fn log_enabled(&self, level: LogLevel) -> bool {
        level != LogLevel::None
    }
}
