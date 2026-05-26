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

## Outbound HTTP extension (`http_fetch`)

The base http-wasm spec gives a guest no way to make a network call. This crate
adds an **optional, non-standard** host function, `http_fetch`, for middleware
that needs it (e.g. a bouncer querying a decision API). A guest using it is no
longer portable to a vanilla http-wasm host — opting in is explicit.

The crate does no I/O itself: you implement [`Fetcher`] (bridging to your own
HTTP client) and enable it per plugin. Your implementation owns the policy —
URL allow-listing (anti-SSRF), timeouts, TLS.

```rust
use std::sync::Arc;
use http_wasm_host::{Plugin, Limits, Fetcher, FetchRequest, FetchResponse};

struct MyFetcher;
impl Fetcher for MyFetcher {
    fn fetch(&self, req: FetchRequest) -> Result<FetchResponse, String> {
        // validate req.url against an allow-list, then perform it
        # let _ = req;
        Ok(FetchResponse { status: 200, headers: vec![], body: vec![] })
    }
}

let plugin = Plugin::from_bytes(&wasm_bytes, Limits::default())?
    .with_fetcher(Arc::new(MyFetcher));
# Ok::<(), http_wasm_host::Error>(())
```

## Fire-and-forget extension (`http_send`)

Where `http_fetch` waits for a response, `http_send` does not: the guest hands
the host a request to perform **later**, and continues immediately. This suits
beacons — analytics, audit events — where putting the network round-trip on the
request path would add latency for no benefit.

You implement [`Sink`] (typically pushing onto a bounded queue drained by a
background worker, with batching and URL allow-listing) and enable it per
plugin:

```rust
use std::sync::Arc;
use http_wasm_host::{Plugin, Limits, Sink, SendOutcome, FetchRequest};

struct MySink; // e.g. wraps a tokio mpsc sender
impl Sink for MySink {
    fn send(&self, req: FetchRequest) -> SendOutcome {
        # let _ = req;
        // enqueue without blocking; drop if full
        SendOutcome::Queued
    }
}

let plugin = Plugin::from_bytes(&wasm_bytes, Limits::default())?
    .with_sink(Arc::new(MySink));
# Ok::<(), http_wasm_host::Error>(())
```

## Writing a guest

A guest imports the host functions from the module `http_handler` and exports
`handle_request` (returning the `ctx_next` i64) and optionally
`handle_response`. You can write one by hand against the raw ABI (see
`tests/fixtures/guest-headers`) or use a guest SDK such as
[`http-wasm-guest`](https://crates.io/crates/http-wasm-guest) for Rust.

## License

MIT — see [LICENSE](LICENSE).
