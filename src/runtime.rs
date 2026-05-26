//! The wasmtime-backed runtime: compiles a guest once into a [`Plugin`], then
//! drives the http-wasm phases against a [`Host`] for each request.

use std::time::Duration;

use wasmtime::{Caller, Engine, Linker, Memory, Module, Store};

use crate::abi::{self, HeaderKind, LogLevel};
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

/// A compiled guest, ready to run against many requests. Cheap to clone the
/// underlying module; compile once and reuse.
pub struct Plugin {
    engine: Engine,
    module: Module,
    limits: Limits,
}

/// The store payload: a raw pointer to the embedder's `Host` for the duration
/// of one phase call, plus the resolved guest memory. The pointer is only ever
/// dereferenced synchronously inside host functions while `run_phase` holds the
/// borrow, so it never dangles.
struct StoreData {
    host: *mut (dyn Host + 'static),
    memory: Option<Memory>,
    limiter: MemLimiter,
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
        let module = Module::new(&engine, wasm).map_err(|e| Error::Load(e.to_string()))?;
        Ok(Self {
            engine,
            module,
            limits,
        })
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

    /// Shared setup: build a store wired to `host`, instantiate, resolve memory,
    /// arm the timeout, then run `call`.
    fn run_phase<R>(
        &self,
        host: &mut dyn Host,
        call: impl FnOnce(&wasmtime::Instance, &mut Store<StoreData>) -> Result<R, Error>,
    ) -> Result<R, Error> {
        // Erase the lifetime to a raw pointer: the borrow is live for this whole
        // function, and host functions only run synchronously within `call`.
        let host_ptr: *mut (dyn Host + 'static) =
            unsafe { std::mem::transmute::<*mut dyn Host, *mut (dyn Host + 'static)>(host) };

        let data = StoreData {
            host: host_ptr,
            memory: None,
            limiter: MemLimiter {
                max: self.limits.max_memory_bytes,
            },
        };
        let mut store = Store::new(&self.engine, data);
        store.limiter(|d| &mut d.limiter);
        store.set_epoch_deadline(1);

        let mut linker: Linker<StoreData> = Linker::new(&self.engine);
        register_host_functions(&mut linker)?;

        let instance = linker
            .instantiate(&mut store, &self.module)
            .map_err(|e| Error::Load(e.to_string()))?;

        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or(Error::MissingExport("memory"))?;
        store.data_mut().memory = Some(memory);

        // Arm the timeout: a background thread bumps the engine epoch once the
        // budget elapses, tripping the deadline mid-execution.
        let engine = self.engine.clone();
        let timeout = self.limits.timeout;
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let watchdog = std::thread::spawn(move || {
            if rx.recv_timeout(timeout).is_err() {
                engine.increment_epoch();
            }
        });

        let result = call(&instance, &mut store);

        let _ = tx.send(()); // tell the watchdog we finished in time
        let _ = watchdog.join();
        result
    }
}

/// Borrow the embedder's `Host` out of the store payload. Safe within a host
/// function: `run_phase` keeps the original `&mut dyn Host` alive and never
/// aliases it (host functions are synchronous and non-reentrant here).
fn host_of<'a>(caller: &'a mut Caller<'_, StoreData>) -> &'a mut dyn Host {
    let ptr = caller.data().host;
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
