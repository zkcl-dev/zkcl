const HOST_MODULE = "zkcl.host";
const MAX_TIMEOUT_MS = 2_147_483_647;
const CANCEL_SLICE_MS = 16;
const NS_PER_MS = 1_000_000;

async function sleep(nanos, run) {
  if (nanos < 0n) return -1;
  const signal = run?.signal;
  const deadline = performance.now() + Number(nanos) / NS_PER_MS;
  try {
    for (;;) {
      const cancellable = signal !== undefined && !run.cancelDelivered;
      if (cancellable && signal.aborted) {
        // Cleanups can themselves sleep; deliver a request just once per run.
        // An inactive invocation is also a successful wake, not a timer error.
        if (run.cancel() === -1) return -1;
        run.cancelDelivered = true;
        return 0;
      }
      const remaining = deadline - performance.now();
      if (remaining <= 0) return 0;
      const limit = cancellable ? CANCEL_SLICE_MS : MAX_TIMEOUT_MS;
      const delay = Math.min(limit, Math.ceil(remaining));
      await new Promise((resolve) => setTimeout(resolve, delay));
    }
  } catch {
    return -1;
  }
}

function syncStatus(callback, success) {
  try {
    callback();
    return success;
  } catch {
    return -1;
  }
}

/** Instantiate an application built by `zkcl build --target wasm32-unknown-unknown`.
 *  `write` receives a channel number (1 or 2) and an owned UTF-8 byte chunk.
 *  `write` and `flush` are synchronous. `run({ signal })` resolves to the exit status.
 *  An optional AbortSignal requests Lisp cleanup while the guest waits on a timer.
 */
export async function instantiate(module, { args = [], write, flush = () => {} } = {}) {
  if (typeof write !== "function" || typeof flush !== "function") {
    throw new TypeError("zkcl requires synchronous output callbacks");
  }
  const encoder = new TextEncoder();
  const argv = Array.from(args, (arg) => encoder.encode(arg));
  const started = performance.now();
  let memory;
  let activeRun;
  const bytes = (offset, length) => new Uint8Array(memory.buffer, offset >>> 0, length >>> 0);
  const compiled = module instanceof WebAssembly.Module ? module : await WebAssembly.compile(module);
  const imports = WebAssembly.Module.imports(compiled);
  const usesSleep = imports.some(({ module, name }) => module === HOST_MODULE && name === "sleep_ns");
  const canSuspend = usesSleep && typeof WebAssembly.Suspending === "function" && typeof WebAssembly.promising === "function";
  const instance = await WebAssembly.instantiate(compiled, {
    [HOST_MODULE]: {
      write(channel, offset, length) {
        if (channel !== 1 && channel !== 2) return -1;
        const chunk = bytes(offset, length).slice();
        return syncStatus(() => write(channel, chunk), length);
      },
      flush(channel) {
        if (channel !== 1 && channel !== 2) return -1;
        return syncStatus(() => flush(channel), 0);
      },
      arg_count: () => argv.length,
      arg_len: (index) => argv[index]?.length ?? -1,
      arg_read(index, offset, capacity) {
        const arg = argv[index];
        if (arg === undefined || arg.length > (capacity >>> 0)) return -1;
        bytes(offset, arg.length).set(arg);
        return arg.length;
      },
      wall_time_secs: () => BigInt(Math.floor(Date.now() / 1_000)),
      monotonic_nanos: () => BigInt(Math.floor((performance.now() - started) * 1_000_000)),
      fill_entropy(offset, length) {
        const buffer = bytes(offset, length);
        return syncStatus(() => {
          for (let start = 0; start < buffer.length; start += 65_536) {
            globalThis.crypto.getRandomValues(buffer.subarray(start, start + 65_536));
          }
        }, 0);
      },
      sleep_ns: canSuspend
        ? new WebAssembly.Suspending((nanos) => sleep(nanos, activeRun))
        : (nanos) => nanos === 0n ? 0 : -1,
    },
  });
  memory = instance.exports.memory;
  const { run: entry, cancel } = instance.exports;
  if (!(memory instanceof WebAssembly.Memory) || typeof entry !== "function" || typeof cancel !== "function") {
    throw new TypeError("zkcl application must export memory, run, and cancel");
  }
  const invoke = canSuspend ? WebAssembly.promising(entry) : entry;
  return {
    async run({ signal } = {}) {
      if (activeRun) throw new Error("zkcl application is already running");
      signal?.throwIfAborted();
      activeRun = { signal, cancel, cancelDelivered: false };
      try {
        return await invoke();
      } finally {
        activeRun = undefined;
      }
    },
  };
}
