import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { instantiate } from "../../zkcl-wasm/js/host.mjs";

const stdout = [];
const stderr = [];
const flushed = [];
const module = await WebAssembly.compile(await readFile(process.argv[2]));
assert.deepEqual(WebAssembly.Module.exports(module), [
  { name: "memory", kind: "memory" },
  { name: "run", kind: "function" },
  { name: "cancel", kind: "function" },
]);
const app = await instantiate(module, {
  args: ["雪🙂"],
  write(channel, chunk) {
    (channel === 1 ? stdout : stderr).push(chunk);
  },
  flush(channel) {
    flushed.push(channel);
  },
});
const status = app.run();
await assert.rejects(app.run(), /already running/);
assert.equal(await status, 17, Buffer.concat(stderr).toString());
assert.deepEqual(flushed, [1]);
assert.equal(Buffer.concat(stdout).toString(), "waiting 雪🙂\ncleanup\n");
assert.equal(stderr.length, 0);

assert.equal(await app.run(), 17, Buffer.concat(stderr).toString());
assert.deepEqual(flushed, [1, 1]);
assert.equal(Buffer.concat(stdout).toString(), "waiting 雪🙂\ncleanup\n".repeat(2));
assert.equal(stderr.length, 0);

const syncSleepOutput = "sync finished\nwaiting --sync-sleep\ncleanup\n";

async function spuriousHostWakePreservesSynchronousSleep() {
  const output = [[], [], []];
  const requested = [];
  const arg = new TextEncoder().encode("--sync-sleep");
  let nanos = 0n;
  const bytes = (offset, len) => new Uint8Array(raw.exports.memory.buffer, offset, len);
  const raw = await WebAssembly.instantiate(module, { "zkcl.host": {
    write(channel, offset, len) { output[channel].push(Buffer.from(bytes(offset, len))); return len; },
    flush: () => 0,
    arg_count: () => 1,
    arg_len: () => arg.length,
    arg_read(index, offset, len) { bytes(offset, len).set(arg); return arg.length; },
    wall_time_secs: () => 2_000_000_000n,
    monotonic_nanos: () => nanos,
    fill_entropy(offset, len) { bytes(offset, len).fill(1); return 0; },
    sleep_ns: new WebAssembly.Suspending(async (delay) => {
      requested.push(delay);
      nanos += requested.length <= 2 ? 7_000_000n : delay;
      return 0;
    }),
  } });
  assert.equal(raw.exports.cancel(), 0);
  assert.equal(await WebAssembly.promising(raw.exports.run)(), 17, Buffer.concat(output[2]).toString());
  assert.deepEqual(requested.slice(0, 3), [25_000_000n, 18_000_000n, 11_000_000n]);
  assert.equal(Buffer.concat(output[1]).toString(), syncSleepOutput);
  assert.equal(raw.exports.cancel(), 0);
}

await spuriousHostWakePreservesSynchronousSleep();

// Control both the host timer and monotonic clock. Leaving the original
// 60-second timer pending would fail here rather than stall the test.
const originalPerformance = Object.getOwnPropertyDescriptor(globalThis, "performance");
const originalSetTimeout = globalThis.setTimeout;
const nextTurn = () => new Promise((resolve) => setImmediate(resolve));
const timers = [];
let now = 0;
Object.defineProperty(globalThis, "performance", {
  configurable: true,
  value: { now: () => now },
});
globalThis.setTimeout = (resolve, delay) => {
  timers.push({ resolve, delay, deadline: now + delay });
};
const fire = () => {
  const { resolve, deadline } = timers.shift();
  now = deadline;
  resolve();
};

async function recordedApp(compiled, mode) {
  const output = [[], [], []];
  const app = await instantiate(compiled, {
    args: [mode],
    write(channel, chunk) { output[channel].push(chunk); },
  });
  return { app, output, text: (channel) => Buffer.concat(output[channel]).toString() };
}

async function cancellationJustAfterSchedulerPoll() {
  const { app, text } = await recordedApp(module, "--cancel-after-poll");
  const controller = new AbortController();
  const status = app.run({ signal: controller.signal });
  await nextTurn();
  assert.equal(text(1), "parked\n");
  assert.deepEqual(timers.map(({ delay }) => delay), [5]);
  fire();
  await nextTurn();
  // The first host wait returned successfully, the scheduler already polled
  // with no request, and the Lisp task registered a second wait.
  assert.equal(text(1), "parked\nreparked\n");
  assert.deepEqual(timers.map(({ delay }) => delay), [16]);
  const requestedAt = now;
  controller.abort();
  await nextTurn();
  assert.equal(text(1), "parked\nreparked\n");
  fire();
  await nextTurn();
  assert.equal(now - requestedAt, 16);
  assert.equal(text(1), "parked\nreparked\ncleanup started\n");
  assert.deepEqual(timers.map(({ delay }) => delay), [25]);
  fire();
  assert.equal(await status, 1);
  assert.match(text(2), /evaluation cancelled/);
  assert.equal(text(1), "parked\nreparked\ncleanup started\ncleanup finished\n");
  assert.deepEqual(timers, []);
}

async function inactiveCancellationIsNotATimerFailure() {
  const inactive = await WebAssembly.compile(await readFile(process.argv[3]));
  const { app, text } = await recordedApp(inactive, "--sync-sleep");
  const controller = new AbortController();
  const status = app.run({ signal: controller.signal });
  await nextTurn();
  assert.deepEqual(timers.map(({ delay }) => delay), [16]);
  controller.abort();
  fire();
  await nextTurn();
  assert.deepEqual(timers.map(({ delay }) => delay), [9]);
  for (let i = 0; i < 16 && timers.length !== 0; i++) {
    fire();
    await nextTurn();
  }
  assert.deepEqual(timers, []);
  assert.equal(await status, 17, text(2));
  assert.equal(text(1), syncSleepOutput);
}

try {
  const { app, output, text } = await recordedApp(module, "--external-cancel");
  const rejected = new AbortController();
  rejected.abort();
  await assert.rejects(app.run({ signal: rejected.signal }), { name: "AbortError" });
  assert.equal(text(1), "");
  for (let run = 0; run < 2; run++) {
    output[1].length = 0;
    output[2].length = 0;
    const controller = new AbortController();
    const status = app.run({ signal: controller.signal });
    await nextTurn();
    assert.equal(text(1), "parked\n");
    assert.deepEqual(timers.map(({ delay }) => delay), [16]);
    if (run === 0) {
      fire();
      await nextTurn();
      assert.equal(text(1), "parked\n", "a slice must not cancel an un-aborted run");
      assert.deepEqual(timers.map(({ delay }) => delay), [16]);
    }
    now += run === 0 ? 15 : 8;
    controller.abort();
    controller.abort();
    await nextTurn();
    assert.equal(text(1), "parked\n", "the guest must still be parked before the slice ends");
    fire();
    await nextTurn();
    assert.equal(text(1), "parked\ncleanup started\n");
    assert.deepEqual(timers.map(({ delay }) => delay), [25]);
    fire();
    assert.equal(await status, 1);
    assert.equal(text(1), "parked\ncleanup started\ncleanup finished\n");
    assert.match(text(2), /evaluation cancelled/);
    assert.deepEqual(timers, []);
  }
  await cancellationJustAfterSchedulerPoll();
  await inactiveCancellationIsNotATimerFailure();
} finally {
  Object.defineProperty(globalThis, "performance", originalPerformance);
  globalThis.setTimeout = originalSetTimeout;
}

const suspending = WebAssembly.Suspending;
WebAssembly.Suspending = undefined;
try {
  const output = [[], [], []];
  const app = await instantiate(module, {
    args: ["雪🙂"],
    write(channel, chunk) { output[channel].push(chunk); },
  });
  assert.equal(await app.run(), 1);
  assert.equal(Buffer.concat(output[1]).toString(), "waiting 雪🙂\ncleanup\n");
  const error = Buffer.concat(output[2]).toString();
  assert.match(error, /Wasm host could not schedule SLEEP/);
  assert.match(error, /in WAIT-FOR-TIMER(?: at source byte \d+)? in .*application\.lisp/);
} finally {
  WebAssembly.Suspending = suspending;
}
