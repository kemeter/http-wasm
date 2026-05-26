//! Test guest for the http_send extension. On each request it fires one
//! POST beacon to "http://collector/api/send" and always continues. It never
//! waits for a response (fire-and-forget).

#![no_std]

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    loop {}
}

#[link(wasm_import_module = "http_handler")]
unsafe extern "C" {
    fn http_send(req_ptr: i32, req_len: i32) -> i32;
}

static mut REQ: [u8; 128] = [0; 128];

unsafe fn put(buf: *mut u8, pos: &mut usize, data: &[u8]) {
    for &b in &(data.len() as u32).to_le_bytes() {
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
    unsafe {
        let req = core::ptr::addr_of_mut!(REQ) as *mut u8;
        let mut pos = 0usize;
        put(req, &mut pos, b"POST");
        put(req, &mut pos, b"http://collector/api/send");
        // header count = 0
        for _ in 0..4 {
            *req.add(pos) = 0;
            pos += 1;
        }
        put(req, &mut pos, b"{\"type\":\"event\"}");
        http_send(req as i32, pos as i32);
    }
    1 // continue, regardless of send outcome
}

#[unsafe(no_mangle)]
pub extern "C" fn handle_response(_req_ctx: i32, _is_error: i32) {}
