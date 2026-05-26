//! Test guest for the http_fetch extension. On each request it calls
//! `http_fetch("GET", "http://lapi/decide")` and, if the fetched response
//! status is 403, short-circuits the request with 403; otherwise continues.
//! Mirrors how a bouncer would consult an external decision API.

#![no_std]

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    loop {}
}

const RESPONSE: i32 = 1;

#[link(wasm_import_module = "http_handler")]
unsafe extern "C" {
    fn http_fetch(req_ptr: i32, req_len: i32, resp_ptr: i32, resp_limit: i32) -> i64;
    fn set_status_code(status: i32);
    fn write_body(kind: i32, body: i32, body_len: i32);
}

static mut REQ: [u8; 128] = [0; 128];
static mut RESP: [u8; 256] = [0; 256];

/// Append a length-prefixed (u32 LE) field into `buf` at `pos`.
unsafe fn put(buf: *mut u8, pos: &mut usize, data: &[u8]) {
    let len = data.len() as u32;
    let lb = len.to_le_bytes();
    for &b in &lb {
        unsafe { *buf.add(*pos) = b };
        *pos += 1;
    }
    for &b in data {
        unsafe { *buf.add(*pos) = b };
        *pos += 1;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn handle_request() -> i64 {
    let status = unsafe {
        let req = core::ptr::addr_of_mut!(REQ) as *mut u8;
        let mut pos = 0usize;
        put(req, &mut pos, b"GET"); // method
        put(req, &mut pos, b"http://lapi/decide"); // url
        // header count = 0
        for _ in 0..4 {
            *req.add(pos) = 0;
            pos += 1;
        }
        put(req, &mut pos, b""); // empty body

        let resp = core::ptr::addr_of_mut!(RESP) as *mut u8;
        let packed = http_fetch(req as i32, pos as i32, resp as i32, 256);
        let ok = (packed >> 32) as u32;
        if ok == 0 {
            return 1; // fetch failed -> allow through
        }
        // response encoding starts with status as u32 LE
        let s = core::slice::from_raw_parts(resp, 4);
        u32::from_le_bytes([s[0], s[1], s[2], s[3]])
    };

    if status == 403 {
        unsafe {
            set_status_code(403);
            let body = b"denied by decision api";
            write_body(RESPONSE, body.as_ptr() as i32, body.len() as i32);
        }
        return 0; // stop
    }
    1 // continue
}

#[unsafe(no_mangle)]
pub extern "C" fn handle_response(_req_ctx: i32, _is_error: i32) {}
