//! The wasmtime-backed runtime: compiles a guest once into a [`Plugin`], then
//! drives the http-wasm phases against a [`Host`] for each request.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use wasmtime::{Caller, Engine, Instance, InstancePre, Linker, Memory, Store};

use crate::abi::{self, HeaderKind, LogLevel};
use crate::fetch::{Fetcher, SendOutcome, Sink, codec};
use crate::host::Host;

/// Errors surfaced while loading or running a guest.
#[derive(Debug)]
pub enum Error {
    /// The `.wasm` failed to compile or instantiate.
    Load(String),
    /// A trap, timeout, or memory fault while running a phase.
    Run(String),
    /// The guest is missing a required export (`memory`, `handle_request`, …).
    MissingExport(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Load(e) => write!(f, "failed to load guest: {e}"),
            Error::Run(e) => write!(f, "guest execution failed: {e}"),
            Error::MissingExport(e) => write!(f, "guest is missing required export `{e}`"),
        }
    }
}

impl std::error::Error for Error {}

/// Outcome of the request phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Guest returned `next=1`: proceed to the backend / next handler. Carries
    /// the opaque request context to hand back to `handle_response`.
    Continue(u32),
    /// Guest returned `next=0`: short-circuit. The guest has already written
    /// the response (status/headers/body) via the host.
    Stop,
}

/// Per-invocation limits guarding a misbehaving guest.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Wall-clock budget for a single phase call. Enforced via epoch deadlines.
    pub timeout: Duration,
    /// Hard cap on the guest's linear memory.
    pub max_memory_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_millis(100),
            max_memory_bytes: 16 * 1024 * 1024,
        }
    }
}

/// A compiled guest, ready to run against many requests. Compile once and
/// share it (e.g. behind an `Arc`); phases may run concurrently.
pub struct Plugin {
    engine: Engine,
    /// The guest pre-linked against every host function, so a new instance
    /// only costs the instantiation itself.
    pre: InstancePre<StoreData>,
    limits: Limits,
    fetcher: Option<Arc<dyn Fetcher>>,
    sink: Option<Arc<dyn Sink>>,
    /// Drives the timeout: advances the engine epoch at a fixed tick. Only held
    /// so it stops (on drop) with the plugin.
    _ticker: EpochTicker,
    /// Epoch ticks granted to one phase call (the timeout, in ticks).
    deadline_ticks: u64,
    /// Idle instances kept for reuse, when enabled (see
    /// [`Plugin::with_instance_reuse`]); `max_idle == 0` disables reuse.
    pool: Mutex<Vec<Slot>>,
    max_idle: usize,
}

/// One live guest instance and its store.
struct Slot {
    store: Store<StoreData>,
    instance: Instance,
}

/// The store payload: a raw pointer to the embedder's `Host`, set only for the
/// duration of one phase call, plus the resolved guest memory. The pointer is
/// only ever dereferenced synchronously inside host functions while `run_phase`
/// holds the borrow, and is cleared before the call returns, so it never
/// dangles — even when the store is kept for reuse.
struct StoreData {
    host: Option<*mut (dyn Host + 'static)>,
    memory: Option<Memory>,
    limiter: MemLimiter,
    fetcher: Option<Arc<dyn Fetcher>>,
    sink: Option<Arc<dyn Sink>>,
}

// SAFETY: the only non-Send field is the `Host` pointer, which is `Some` only
// while a phase call runs on the thread that owns the borrow, and is reset to
// `None` before the store goes back to the pool (where another thread may pick
// it up).
unsafe impl Send for StoreData {}

/// Background thread advancing the engine epoch every `tick`. Each call sets
/// its own deadline relative to the current epoch, so a slow call only trips
/// itself: a per-call watchdog bumping the shared epoch would also trip every
/// other call running on the same engine at that moment.
struct EpochTicker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl EpochTicker {
    fn start(engine: Engine, tick: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = std::thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                std::thread::sleep(tick);
                engine.increment_epoch();
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Tick granularity for a given timeout: fine enough to keep the timeout
/// accurate, coarse enough that the ticker costs nothing measurable.
fn tick_for(timeout: Duration) -> Duration {
    (timeout / 20).clamp(Duration::from_millis(1), Duration::from_millis(10))
}

struct MemLimiter {
    max: usize,
}

impl wasmtime::ResourceLimiter for MemLimiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(desired <= self.max)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        _desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(true)
    }
}

impl Plugin {
    /// Compile a guest from `.wasm` bytes (binary or `.wat` text both work).
    pub fn from_bytes(wasm: &[u8], limits: Limits) -> Result<Self, Error> {
        let mut config = wasmtime::Config::new();
        config.epoch_interruption(true);
        let engine = Engine::new(&config).map_err(|e| Error::Load(e.to_string()))?;
        let module =
            wasmtime::Module::new(&engine, wasm).map_err(|e| Error::Load(e.to_string()))?;

        let mut linker: Linker<StoreData> = Linker::new(&engine);
        register_host_functions(&mut linker)?;
        // Always register the outbound extensions so a guest that imports
        // `http_fetch`/`http_send` instantiates whether or not a fetcher/sink is
        // wired. When none is configured the handlers no-op safely (fetch returns
        // an error result, send returns "rejected"), so an event is dropped
        // rather than trapping the whole request. Gating registration on
        // `fetcher/sink.is_some()` left the import unresolved and every request
        // through such a plugin failed to instantiate.
        register_fetch_function(&mut linker)?;
        register_send_function(&mut linker)?;
        register_clock_random_functions(&mut linker)?;
        let pre = linker
            .instantiate_pre(&module)
            .map_err(|e| Error::Load(e.to_string()))?;

        let tick = tick_for(limits.timeout);
        let deadline_ticks = (limits.timeout.as_nanos() / tick.as_nanos()).max(1) as u64 + 1;
        Ok(Self {
            _ticker: EpochTicker::start(engine.clone(), tick),
            engine,
            pre,
            limits,
            fetcher: None,
            sink: None,
            deadline_ticks,
            pool: Mutex::new(Vec::new()),
            max_idle: 0,
        })
    }

    /// Keep up to `max_idle` guest instances alive between phase calls and
    /// reuse them, instead of instantiating a fresh guest for every call.
    ///
    /// This lets a guest keep state across requests — typically a cache built
    /// from its config, which would otherwise be rebuilt on every request.
    /// The guest must then tolerate state left by previous calls (and possibly
    /// by other routes sharing the plugin with a different config). An
    /// instance whose call fails (trap, timeout) is discarded, never reused.
    /// `0` (the default) keeps the fresh-instance-per-call behaviour.
    pub fn with_instance_reuse(mut self, max_idle: usize) -> Self {
        self.max_idle = max_idle;
        self
    }

    /// Enable the outbound-HTTP extension: the guest's `http_fetch` imports are
    /// served by `fetcher`. The `http_fetch` import is always linked, so a guest
    /// that imports it still instantiates without a fetcher; its calls just
    /// return an error result (no request is made) until one is wired here.
    pub fn with_fetcher(mut self, fetcher: Arc<dyn Fetcher>) -> Self {
        self.fetcher = Some(fetcher);
        self
    }

    /// Enable the fire-and-forget extension: the guest's `http_send` imports are
    /// handed to `sink`, which enqueues them without blocking. Like
    /// [`with_fetcher`](Self::with_fetcher), the `http_send` import is always
    /// linked; without a sink the guest still instantiates and its sends are
    /// rejected (the event is dropped) until one is wired here.
    pub fn with_sink(mut self, sink: Arc<dyn Sink>) -> Self {
        self.sink = Some(sink);
        self
    }

    /// Run `handle_request` against `host`. The guest may mutate the request and
    /// either continue or short-circuit with a response.
    pub fn handle_request(&self, host: &mut dyn Host) -> Result<Next, Error> {
        let raw = self.run_phase(host, |instance, store| {
            let func = instance
                .get_typed_func::<(), i64>(&mut *store, "handle_request")
                .map_err(|_| Error::MissingExport("handle_request"))?;
            func.call(&mut *store, ())
                .map_err(|e| Error::Run(e.to_string()))
        })?;
        let (ctx, next) = abi::decode_ctx_next(raw);
        Ok(if next {
            Next::Continue(ctx)
        } else {
            Next::Stop
        })
    }

    /// Run `handle_response` against `host`, passing the `ctx` from the request
    /// phase and whether an upstream error occurred.
    pub fn handle_response(
        &self,
        host: &mut dyn Host,
        ctx: u32,
        is_error: bool,
    ) -> Result<(), Error> {
        self.run_phase(host, |instance, store| {
            // `handle_response` is optional: a guest that only inspects requests
            // need not export it.
            let Ok(func) =
                instance.get_typed_func::<(i32, i32), ()>(&mut *store, "handle_response")
            else {
                return Ok(());
            };
            func.call(&mut *store, (ctx as i32, is_error as i32))
                .map_err(|e| Error::Run(e.to_string()))
        })
    }

    /// Build a new guest instance with its own store.
    fn instantiate(&self) -> Result<Slot, Error> {
        let data = StoreData {
            host: None,
            memory: None,
            limiter: MemLimiter {
                max: self.limits.max_memory_bytes,
            },
            fetcher: self.fetcher.clone(),
            sink: self.sink.clone(),
        };
        let mut store = Store::new(&self.engine, data);
        store.limiter(|d| &mut d.limiter);
        // Instantiation runs the guest's start function: bound it too.
        store.set_epoch_deadline(self.deadline_ticks);

        let instance = self
            .pre
            .instantiate(&mut store)
            .map_err(|e| Error::Load(e.to_string()))?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or(Error::MissingExport("memory"))?;
        store.data_mut().memory = Some(memory);
        Ok(Slot { store, instance })
    }

    /// An idle pooled instance if reuse is enabled and one is available,
    /// otherwise a fresh one.
    fn checkout(&self) -> Result<Slot, Error> {
        if self.max_idle > 0
            && let Some(slot) = self.pool.lock().unwrap_or_else(|e| e.into_inner()).pop()
        {
            return Ok(slot);
        }
        self.instantiate()
    }

    /// Return a healthy instance to the pool (dropped if reuse is off or the
    /// pool is full).
    fn checkin(&self, slot: Slot) {
        if self.max_idle == 0 {
            return;
        }
        let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        if pool.len() < self.max_idle {
            pool.push(slot);
        }
    }

    /// Shared setup: get an instance wired to `host`, arm the timeout, then run
    /// `call`.
    fn run_phase<R>(
        &self,
        host: &mut dyn Host,
        call: impl FnOnce(&Instance, &mut Store<StoreData>) -> Result<R, Error>,
    ) -> Result<R, Error> {
        // Erase the lifetime to a raw pointer: the borrow is live for this whole
        // function, host functions only run synchronously within `call`, and the
        // pointer is cleared before returning.
        let host_ptr: *mut (dyn Host + 'static) =
            unsafe { std::mem::transmute::<*mut dyn Host, *mut (dyn Host + 'static)>(host) };

        let mut slot = self.checkout()?;
        slot.store.data_mut().host = Some(host_ptr);
        // Relative to the current epoch: only this call's own budget counts.
        slot.store.set_epoch_deadline(self.deadline_ticks);

        let result = call(&slot.instance, &mut slot.store);

        slot.store.data_mut().host = None;
        if result.is_ok() {
            self.checkin(slot);
        }
        result
    }
}

/// Borrow the embedder's `Host` out of the store payload. Safe within a host
/// function: `run_phase` keeps the original `&mut dyn Host` alive and never
/// aliases it (host functions are synchronous and non-reentrant here).
fn host_of<'a>(caller: &'a mut Caller<'_, StoreData>) -> &'a mut dyn Host {
    let ptr = caller
        .data()
        .host
        .expect("host is set for the duration of a phase call");
    unsafe { &mut *ptr }
}

fn memory_of(caller: &mut Caller<'_, StoreData>) -> Memory {
    caller
        .data()
        .memory
        .expect("memory is set before any guest call runs")
}

/// Read `len` bytes at `ptr` from guest memory into a `Vec`.
fn read_mem(caller: &mut Caller<'_, StoreData>, ptr: i32, len: i32) -> Vec<u8> {
    let mem = memory_of(caller);
    let mut buf = vec![0u8; len.max(0) as usize];
    let _ = mem.read(&caller, ptr as usize, &mut buf);
    buf
}

fn read_str(caller: &mut Caller<'_, StoreData>, ptr: i32, len: i32) -> String {
    String::from_utf8_lossy(&read_mem(caller, ptr, len)).into_owned()
}

/// Write `data` into guest memory at `ptr`, truncated to `limit`. Returns the
/// full length the guest *would* need (so it can re-call with a bigger buffer).
fn write_capped(caller: &mut Caller<'_, StoreData>, ptr: i32, limit: i32, data: &[u8]) -> u32 {
    let full = data.len() as u32;
    let n = (limit.max(0) as usize).min(data.len());
    if n > 0 {
        let mem = memory_of(caller);
        let _ = mem.write(caller, ptr as usize, &data[..n]);
    }
    full
}

/// NUL-terminate and concatenate a list of strings, as the header-name/value
/// ABI expects. Returns `(count, joined_bytes)`.
fn join_nul(items: &[String]) -> (u32, Vec<u8>) {
    let mut out = Vec::new();
    for item in items {
        out.extend_from_slice(item.as_bytes());
        out.push(0);
    }
    (items.len() as u32, out)
}

/// Wire every `http_handler` host function onto the linker.
fn register_host_functions(linker: &mut Linker<StoreData>) -> Result<(), Error> {
    let m = abi::MODULE;
    let wrap = |e: wasmtime::Result<&mut Linker<StoreData>>| -> Result<(), Error> {
        e.map(|_| ()).map_err(|err| Error::Load(err.to_string()))
    };

    wrap(linker.func_wrap(
        m,
        "get_method",
        |mut caller: Caller<'_, StoreData>, buf: i32, limit: i32| -> i32 {
            let v = host_of(&mut caller).method();
            write_capped(&mut caller, buf, limit, v.as_bytes()) as i32
        },
    ))?;
    wrap(linker.func_wrap(
        m,
        "set_method",
        |mut caller: Caller<'_, StoreData>, ptr: i32, len: i32| {
            let v = read_str(&mut caller, ptr, len);
            host_of(&mut caller).set_method(&v);
        },
    ))?;
    wrap(linker.func_wrap(
        m,
        "get_uri",
        |mut caller: Caller<'_, StoreData>, buf: i32, limit: i32| -> i32 {
            let v = host_of(&mut caller).uri();
            write_capped(&mut caller, buf, limit, v.as_bytes()) as i32
        },
    ))?;
    wrap(linker.func_wrap(
        m,
        "set_uri",
        |mut caller: Caller<'_, StoreData>, ptr: i32, len: i32| {
            let v = read_str(&mut caller, ptr, len);
            host_of(&mut caller).set_uri(&v);
        },
    ))?;
    wrap(linker.func_wrap(
        m,
        "get_protocol_version",
        |mut caller: Caller<'_, StoreData>, buf: i32, limit: i32| -> i32 {
            let v = host_of(&mut caller).protocol_version();
            write_capped(&mut caller, buf, limit, v.as_bytes()) as i32
        },
    ))?;
    wrap(linker.func_wrap(
        m,
        "get_source_addr",
        |mut caller: Caller<'_, StoreData>, buf: i32, limit: i32| -> i32 {
            let v = host_of(&mut caller).source_addr();
            write_capped(&mut caller, buf, limit, v.as_bytes()) as i32
        },
    ))?;
    wrap(linker.func_wrap(
        m,
        "get_status_code",
        |mut caller: Caller<'_, StoreData>| -> i32 { host_of(&mut caller).status_code() as i32 },
    ))?;
    wrap(linker.func_wrap(
        m,
        "set_status_code",
        |mut caller: Caller<'_, StoreData>, status: i32| {
            host_of(&mut caller).set_status_code(status as u32);
        },
    ))?;
    wrap(linker.func_wrap(
        m,
        "get_header_names",
        |mut caller: Caller<'_, StoreData>, kind: i32, buf: i32, limit: i32| -> i64 {
            let Some(kind) = HeaderKind::from_i32(kind) else {
                return 0;
            };
            let names = host_of(&mut caller).header_names(kind);
            let (count, bytes) = join_nul(&names);
            let full = write_capped(&mut caller, buf, limit, &bytes);
            abi::encode_count_len(count, full)
        },
    ))?;
    wrap(linker.func_wrap(
        m,
        "get_header_values",
        |mut caller: Caller<'_, StoreData>,
         kind: i32,
         name: i32,
         name_len: i32,
         buf: i32,
         limit: i32|
         -> i64 {
            let Some(kind) = HeaderKind::from_i32(kind) else {
                return 0;
            };
            let name = read_str(&mut caller, name, name_len);
            let values = host_of(&mut caller).header_values(kind, &name);
            let (count, bytes) = join_nul(&values);
            let full = write_capped(&mut caller, buf, limit, &bytes);
            abi::encode_count_len(count, full)
        },
    ))?;
    wrap(linker.func_wrap(
        m,
        "set_header_value",
        |mut caller: Caller<'_, StoreData>,
         kind: i32,
         name: i32,
         name_len: i32,
         value: i32,
         value_len: i32| {
            let Some(kind) = HeaderKind::from_i32(kind) else {
                return;
            };
            let name = read_str(&mut caller, name, name_len);
            let value = read_str(&mut caller, value, value_len);
            host_of(&mut caller).set_header_value(kind, &name, &value);
        },
    ))?;
    wrap(linker.func_wrap(
        m,
        "add_header_value",
        |mut caller: Caller<'_, StoreData>,
         kind: i32,
         name: i32,
         name_len: i32,
         value: i32,
         value_len: i32| {
            let Some(kind) = HeaderKind::from_i32(kind) else {
                return;
            };
            let name = read_str(&mut caller, name, name_len);
            let value = read_str(&mut caller, value, value_len);
            host_of(&mut caller).add_header_value(kind, &name, &value);
        },
    ))?;
    wrap(linker.func_wrap(
        m,
        "remove_header",
        |mut caller: Caller<'_, StoreData>, kind: i32, name: i32, name_len: i32| {
            let Some(kind) = HeaderKind::from_i32(kind) else {
                return;
            };
            let name = read_str(&mut caller, name, name_len);
            host_of(&mut caller).remove_header(kind, &name);
        },
    ))?;
    wrap(linker.func_wrap(
        m,
        "read_body",
        |mut caller: Caller<'_, StoreData>, kind: i32, buf: i32, limit: i32| -> i64 {
            let Some(kind) = HeaderKind::from_i32(kind) else {
                return abi::encode_eof_len(true, 0);
            };
            let data = host_of(&mut caller).read_body(kind, limit.max(0) as usize);
            let eof = data.is_empty();
            let n = write_capped(&mut caller, buf, limit, &data);
            abi::encode_eof_len(eof, n)
        },
    ))?;
    wrap(linker.func_wrap(
        m,
        "write_body",
        |mut caller: Caller<'_, StoreData>, kind: i32, ptr: i32, len: i32| {
            let Some(kind) = HeaderKind::from_i32(kind) else {
                return;
            };
            let data = read_mem(&mut caller, ptr, len);
            host_of(&mut caller).write_body(kind, &data);
        },
    ))?;
    wrap(linker.func_wrap(
        m,
        "get_config",
        |mut caller: Caller<'_, StoreData>, buf: i32, limit: i32| -> i32 {
            let cfg = host_of(&mut caller).config();
            write_capped(&mut caller, buf, limit, &cfg) as i32
        },
    ))?;
    wrap(linker.func_wrap(
        m,
        "enable_features",
        // We accept whatever the guest requests and echo it back as supported.
        |_caller: Caller<'_, StoreData>, features: i32| -> i32 { features },
    ))?;
    wrap(linker.func_wrap(
        m,
        "log",
        |mut caller: Caller<'_, StoreData>, level: i32, ptr: i32, len: i32| {
            let msg = read_str(&mut caller, ptr, len);
            host_of(&mut caller).log(LogLevel::from_i32(level), &msg);
        },
    ))?;
    wrap(linker.func_wrap(
        m,
        "log_enabled",
        |mut caller: Caller<'_, StoreData>, level: i32| -> i32 {
            host_of(&mut caller).log_enabled(LogLevel::from_i32(level)) as i32
        },
    ))?;

    Ok(())
}

/// Register the optional `http_fetch` extension (module `http_handler`).
///
/// ABI: `http_fetch(req_ptr, req_len, resp_ptr, resp_limit) -> i64`. The guest
/// writes a length-prefixed request (see [`crate::fetch::codec`]) at `req_ptr`;
/// the host performs it via the [`Fetcher`] and writes the encoded response at
/// `resp_ptr`. The i64 return packs `(ok, len)`: `ok` (high 32 bits) is 1 on
/// success / 0 on denial or error, `len` (low 32 bits) is the full encoded
/// response length — if it exceeds `resp_limit` the guest re-calls with a
/// bigger buffer, like the other sized ABI calls.
fn register_fetch_function(linker: &mut Linker<StoreData>) -> Result<(), Error> {
    linker
        .func_wrap(
            abi::MODULE,
            "http_fetch",
            |mut caller: Caller<'_, StoreData>,
             req_ptr: i32,
             req_len: i32,
             resp_ptr: i32,
             resp_limit: i32|
             -> i64 {
                let raw = read_mem(&mut caller, req_ptr, req_len);
                let Some(request) = codec::decode_request(&raw) else {
                    return abi::encode_count_len(0, 0);
                };
                let Some(fetcher) = caller.data().fetcher.clone() else {
                    return abi::encode_count_len(0, 0);
                };
                match fetcher.fetch(request) {
                    Ok(resp) => {
                        let encoded = codec::encode_response(&resp);
                        let full = write_capped(&mut caller, resp_ptr, resp_limit, &encoded);
                        abi::encode_count_len(1, full)
                    }
                    Err(_) => abi::encode_count_len(0, 0),
                }
            },
        )
        .map(|_| ())
        .map_err(|err| Error::Load(err.to_string()))
}

/// Register the optional `http_send` extension (module `http_handler`).
///
/// ABI: `http_send(req_ptr, req_len) -> i32`. The guest writes a length-prefixed
/// request (same codec as `http_fetch`) at `req_ptr`; the host hands it to the
/// [`Sink`] which enqueues it without blocking. Returns the [`SendOutcome`] as
/// an i32: 0 = queued, 1 = queue full, 2 = rejected, 3 = malformed request.
fn register_send_function(linker: &mut Linker<StoreData>) -> Result<(), Error> {
    linker
        .func_wrap(
            abi::MODULE,
            "http_send",
            |mut caller: Caller<'_, StoreData>, req_ptr: i32, req_len: i32| -> i32 {
                let raw = read_mem(&mut caller, req_ptr, req_len);
                let Some(request) = codec::decode_request(&raw) else {
                    return 3; // malformed
                };
                let Some(sink) = caller.data().sink.clone() else {
                    return 2; // no sink configured: reject (drop the event)
                };
                match sink.send(request) {
                    SendOutcome::Queued => 0,
                    SendOutcome::QueueFull => 1,
                    SendOutcome::Rejected => 2,
                }
            },
        )
        .map(|_| ())
        .map_err(|err| Error::Load(err.to_string()))
}

/// Register the optional clock and randomness extensions (module
/// `http_handler`).
///
/// A guest built for `wasm32-unknown-unknown` has no clock and no entropy
/// source of its own, and every phase runs in a fresh instance, so it cannot
/// even carry a PRNG state across requests. These functions give it both,
/// served by the host OS. Like the outbound extensions they are always linked
/// and need no embedder wiring.
///
/// ABI:
/// - `get_time_unix_nanos() -> i64`: wall-clock time as nanoseconds since the
///   Unix epoch (0 if the host clock is before the epoch).
/// - `get_random(buf, len) -> i32`: fill `len` bytes at `buf` with
///   cryptographically secure random bytes. Returns 1 on success, 0 if the
///   host could not produce entropy or the range is outside guest memory (the
///   buffer is then left untouched).
fn register_clock_random_functions(linker: &mut Linker<StoreData>) -> Result<(), Error> {
    linker
        .func_wrap(
            abi::MODULE,
            "get_time_unix_nanos",
            |_caller: Caller<'_, StoreData>| -> i64 {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
                    .unwrap_or(0)
            },
        )
        .map_err(|err| Error::Load(err.to_string()))?;
    linker
        .func_wrap(
            abi::MODULE,
            "get_random",
            |mut caller: Caller<'_, StoreData>, buf: i32, len: i32| -> i32 {
                let Ok(len) = usize::try_from(len) else {
                    return 0;
                };
                let mem = memory_of(&mut caller);
                let in_bounds = (buf as u32 as usize)
                    .checked_add(len)
                    .is_some_and(|end| end <= mem.data_size(&caller));
                if !in_bounds {
                    return 0;
                }
                let mut bytes = vec![0u8; len];
                if getrandom::fill(&mut bytes).is_err() {
                    return 0;
                }
                mem.write(&mut caller, buf as u32 as usize, &bytes).is_ok() as i32
            },
        )
        .map(|_| ())
        .map_err(|err| Error::Load(err.to_string()))
}
