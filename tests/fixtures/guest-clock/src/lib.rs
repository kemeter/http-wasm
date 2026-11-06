//! Test guest for the clock and randomness extensions. On each request it
//! reports, as response headers:
//! - `x-time`: `get_time_unix_nanos()` in decimal;
//! - `x-random`: 16 bytes from `get_random`, hex-encoded;
//! - `x-random-ok`: the return code of that call;
//! - `x-random-oob`: the return code of a call whose range is past the end of
//!   guest memory (must be 0).
//!
//! It always continues.

#![no_std]

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    loop {}
}

#[link(wasm_import_module = "http_handler")]
unsafe extern "C" {
    fn get_time_unix_nanos() -> i64;
    fn get_random(buf: i32, len: i32) -> i32;
    fn set_header_value(kind: i32, name: i32, name_len: i32, value: i32, value_len: i32);
}

const RESPONSE: i32 = 1;

static mut RANDOM: [u8; 16] = [0; 16];
static mut TEXT: [u8; 32] = [0; 32];

unsafe fn set_header(name: &[u8], value: &[u8]) {
    unsafe {
        set_header_value(
            RESPONSE,
            name.as_ptr() as i32,
            name.len() as i32,
            value.as_ptr() as i32,
            value.len() as i32,
        )
    };
}

/// Format `n` in decimal into TEXT and return the used slice.
unsafe fn decimal(mut n: u64) -> &'static [u8] {
    let text = unsafe { &mut *core::ptr::addr_of_mut!(TEXT) };
    let mut pos = text.len();
    loop {
        pos -= 1;
        text[pos] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    &text[pos..]
}

#[unsafe(no_mangle)]
pub extern "C" fn handle_request() -> i64 {
    unsafe {
        let now = get_time_unix_nanos();
        set_header(b"x-time", decimal(now as u64));

        let random = &mut *core::ptr::addr_of_mut!(RANDOM);
        let ok = get_random(random.as_mut_ptr() as i32, random.len() as i32);
        set_header(b"x-random-ok", decimal(ok as u64));

        let text = &mut *core::ptr::addr_of_mut!(TEXT);
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for (i, b) in random.iter().enumerate() {
            text[2 * i] = HEX[(b >> 4) as usize];
            text[2 * i + 1] = HEX[(b & 0xf) as usize];
        }
        set_header(b"x-random", &text[..]);

        // A range ending past linear memory must be refused, not trap.
        let oob = get_random(i32::MAX - 4, 16);
        set_header(b"x-random-oob", decimal(oob as u64));
    }
    1
}

#[unsafe(no_mangle)]
pub extern "C" fn handle_response(_req_ctx: i32, _is_error: i32) {}
