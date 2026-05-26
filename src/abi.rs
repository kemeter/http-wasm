//! Constants and encoding helpers from the http-wasm handler ABI.
//!
//! Reference: <https://http-wasm.io/http-handler-abi/>. The ABI is a *core*
//! WebAssembly module ABI (not the component model): the guest imports host
//! functions from the module named [`MODULE`] and exports `handle_request`,
//! `handle_response` and its `memory`.

/// Name of the host module the guest imports its functions from.
pub const MODULE: &str = "http_handler";

/// Which HTTP message a header/body/trailer operation targets. Passed as the
/// `kind` parameter to header and body functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderKind {
    Request,
    Response,
    RequestTrailers,
    ResponseTrailers,
}

impl HeaderKind {
    /// Decode the raw `kind` i32 from the ABI. Unknown values map to `None`.
    pub fn from_i32(v: i32) -> Option<Self> {
        match v {
            0 => Some(Self::Request),
            1 => Some(Self::Response),
            2 => Some(Self::RequestTrailers),
            3 => Some(Self::ResponseTrailers),
            _ => None,
        }
    }
}

/// Log levels, matching the http-wasm `log` ABI. Mirrors the spec's signed
/// integer levels (debug is negative).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
    None,
}

impl LogLevel {
    /// Decode the raw level i32 from the ABI. Per the spec: debug=-1, info=0,
    /// warn=1, error=2, none=3. Unknown values fall back to `Info`.
    pub fn from_i32(v: i32) -> Self {
        match v {
            i32::MIN..=-1 => Self::Debug,
            0 => Self::Info,
            1 => Self::Warn,
            2 => Self::Error,
            _ => Self::None,
        }
    }
}

/// Feature flags negotiated via `enable_features`. Bitwise-ORed together.
pub mod features {
    /// Buffer the request body so it can be read in full (and re-read).
    pub const BUFFER_REQUEST: i32 = 1 << 0;
    /// Buffer the response body so it can be inspected in `handle_response`.
    pub const BUFFER_RESPONSE: i32 = 1 << 1;
    /// Expose request/response trailers.
    pub const TRAILERS: i32 = 1 << 2;
}

/// Encode a `(count, len)` pair into the i64 the ABI returns from
/// `get_header_names` / `get_header_values`: count in the upper 32 bits, byte
/// length in the lower 32 bits.
pub fn encode_count_len(count: u32, len: u32) -> i64 {
    ((count as i64) << 32) | (len as i64)
}

/// Encode an `(eof, len)` pair into the i64 returned by `read_body`: the EOF
/// flag in the upper 32 bits, bytes read in the lower 32 bits.
pub fn encode_eof_len(eof: bool, len: u32) -> i64 {
    ((eof as i64) << 32) | (len as i64)
}

/// Encode the `ctx_next` i64 returned by the guest's `handle_request`: the
/// opaque request context in the upper 32 bits, the "proceed" flag (1 = call
/// the next handler) in the lower 32 bits.
pub fn decode_ctx_next(v: i64) -> (u32, bool) {
    let ctx = (v >> 32) as u32;
    let next = (v as u32) & 1 == 1;
    (ctx, next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_len_roundtrip() {
        let v = encode_count_len(3, 42);
        assert_eq!((v >> 32) as u32, 3);
        assert_eq!(v as u32, 42);
    }

    #[test]
    fn eof_len_sets_high_bit_only_when_eof() {
        assert_eq!(encode_eof_len(false, 10) >> 32, 0);
        assert_eq!(encode_eof_len(true, 10) >> 32, 1);
        assert_eq!(encode_eof_len(true, 10) as u32, 10);
    }

    #[test]
    fn ctx_next_decodes_both_halves() {
        let raw = ((7i64) << 32) | 1;
        assert_eq!(decode_ctx_next(raw), (7, true));
        let raw_stop = (9i64) << 32; // next flag = 0
        assert_eq!(decode_ctx_next(raw_stop), (9, false));
    }

    #[test]
    fn header_kind_decodes_known_values() {
        assert_eq!(HeaderKind::from_i32(0), Some(HeaderKind::Request));
        assert_eq!(HeaderKind::from_i32(1), Some(HeaderKind::Response));
        assert_eq!(HeaderKind::from_i32(99), None);
    }

    #[test]
    fn log_level_debug_is_negative() {
        assert_eq!(LogLevel::from_i32(-1), LogLevel::Debug);
        assert_eq!(LogLevel::from_i32(0), LogLevel::Info);
        assert_eq!(LogLevel::from_i32(2), LogLevel::Error);
    }
}
