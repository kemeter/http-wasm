//! Minimal http-wasm guest used by the host integration tests. It speaks the
//! raw `http_handler` ABI directly (no guest SDK) so the test exercises exactly
//! the host functions this crate implements.
//!
//! Behaviour:
//! - reads request header `x-block`
//! - if it equals `yes`: set status 403, write a body, and short-circuit
//!   (`handle_request` returns next=0)
//! - otherwise: add response header `x-greeted: true` and continue (next=1)

#![no_std]

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    loop {}
}

const REQUEST: i32 = 0;
const RESPONSE: i32 = 1;

#[link(wasm_import_module = "http_handler")]
unsafe extern "C" {
    fn get_header_values(kind: i32, name: i32, name_len: i32, buf: i32, buf_limit: i32) -> i64;
    fn add_header_value(kind: i32, name: i32, name_len: i32, value: i32, value_len: i32);
    fn set_status_code(status: i32);
    fn write_body(kind: i32, body: i32, body_len: i32);
}

static mut SCRATCH: [u8; 256] = [0; 256];

#[unsafe(no_mangle)]
pub extern "C" fn handle_request() -> i64 {
    let name = b"x-block";
    let blocked = unsafe {
        let scratch = core::ptr::addr_of_mut!(SCRATCH) as *mut u8;
        let res = get_header_values(
            REQUEST,
            name.as_ptr() as i32,
            name.len() as i32,
            scratch as i32,
            256,
        );
        let len = (res & 0xffff_ffff) as usize;
        // values are NUL-terminated; compare the first one against "yes"
        let bytes = core::slice::from_raw_parts(scratch, len);
        bytes.starts_with(b"yes")
    };

    if blocked {
        unsafe {
            set_status_code(403);
            let body = b"blocked by guest";
            write_body(RESPONSE, body.as_ptr() as i32, body.len() as i32);
        }
        // upper 32 bits = ctx (0), lower = next flag (0 => stop)
        return 0;
    }

    unsafe {
        let hname = b"x-greeted";
        let hval = b"true";
        add_header_value(
            RESPONSE,
            hname.as_ptr() as i32,
            hname.len() as i32,
            hval.as_ptr() as i32,
            hval.len() as i32,
        );
    }
    // ctx = 0, next = 1 => continue
    1
}

#[unsafe(no_mangle)]
pub extern "C" fn handle_response(_req_ctx: i32, _is_error: i32) {}
