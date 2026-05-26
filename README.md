# http-wasm-host

A Rust **host** implementation of the [http-wasm](https://http-wasm.io) handler
ABI — run portable HTTP middleware compiled to WebAssembly, from Rust.

http-wasm defines a small, language-neutral ABI for HTTP middleware: a *guest*
`.wasm` exports `handle_request` / `handle_response`, and the *host* exposes the
current request and response through a fixed set of functions. Guests can be
written in any language that targets the ABI (Rust, TinyGo, …) and run unchanged
on any conforming host.

The reference host implementation is in Go (`http-wasm-host-go`). **This crate
is the host for Rust** — load a guest and run it against any HTTP stack by
implementing one trait.

## Status

Early. The core request/response surface works end to end (method, URI,
headers, status, body, short-circuit, request→response context). Not yet
implemented: outbound HTTP from the guest (not part of the spec), trailers, and
streaming bodies beyond the buffered path.

## Usage

```rust
use http_wasm_host::{Plugin, Limits, Next, Host, HeaderKind};

// 1. Implement `Host` over your HTTP types (axum, hyper, your own…).
//    See `tests/integration.rs` for a complete in-memory example.

// 2. Compile a guest once, reuse across requests.
let plugin = Plugin::from_bytes(&wasm_bytes, Limits::default())?;

// 3. Drive the phases per request.
match plugin.handle_request(&mut host)? {
    Next::Continue(ctx) => {
        // forward to the backend, then run the response phase:
        plugin.handle_response(&mut host, ctx, /* is_error = */ false)?;
    }
    Next::Stop => {
        // the guest wrote the response itself (status/headers/body)
    }
}
# Ok::<(), http_wasm_host::Error>(())
```

## Safety guards

`Limits` caps each invocation:

- `timeout` — wall-clock budget per phase, enforced via wasmtime epoch
  interruption (a watchdog trips the deadline if a guest spins).
- `max_memory_bytes` — hard cap on guest linear memory growth.

## Writing a guest

A guest imports the host functions from the module `http_handler` and exports
`handle_request` (returning the `ctx_next` i64) and optionally
`handle_response`. You can write one by hand against the raw ABI (see
`tests/fixtures/guest-headers`) or use a guest SDK such as
[`http-wasm-guest`](https://crates.io/crates/http-wasm-guest) for Rust.

## License

MIT — see [LICENSE](LICENSE).
