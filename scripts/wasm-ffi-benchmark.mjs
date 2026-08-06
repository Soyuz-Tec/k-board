import { readFile } from "node:fs/promises";
import { performance } from "node:perf_hooks";

const path = process.env.KBOARD_WASM ?? "target/wasm32-unknown-unknown/release/kboard.wasm";
const bytes = await readFile(path);
const { instance } = await WebAssembly.instantiate(bytes, {});
const api = instance.exports;
if (api.kb_abi_version() !== 3) throw new Error("unexpected ABI");

const encoder = new TextEncoder();
const write = (text, use) => {
  const encoded = encoder.encode(text);
  const pointer = api.kb_alloc(encoded.length);
  new Uint8Array(api.memory.buffer).set(encoded, pointer);
  try {
    return use(pointer, encoded.length);
  } finally {
    api.kb_free(pointer, encoded.length);
  }
};

const scope = "benchmark/wasm-ffi";
const handle = write(scope, (pointer, length) => api.kb_open(pointer, length, 901n));
for (let index = 0; index < 1000; index += 1) {
  const command = JSON.stringify({
    cmd: "add",
    kind: "rectangle",
    x: index,
    y: index % 97,
    w: 120,
    h: 80,
  });
  const status = write(command, (pointer, length) =>
    api.kb_exec(handle, pointer, length, 1_000 + index),
  );
  if (status !== 0) throw new Error(`fixture failed at ${index}: ${status}`);
}

for (let index = 0; index < 20; index += 1) api.kb_scene(handle);
const samples = [];
for (let index = 0; index < 100; index += 1) {
  const started = performance.now();
  if (api.kb_scene(handle) !== 0) throw new Error("scene failed");
  samples.push((performance.now() - started) * 1000);
}
samples.sort((left, right) => left - right);
const percentile = (p) => samples[Math.min(samples.length - 1, Math.ceil(samples.length * p) - 1)];
const result = {
  benchmark: "wasm/ffi_scene/1000",
  artifactBytes: bytes.length,
  samples: samples.length,
  medianMicros: percentile(0.5),
  p95Micros: percentile(0.95),
  p99Micros: percentile(0.99),
  maxMicros: samples.at(-1),
};
console.log(JSON.stringify(result, null, 2));
api.kb_close(handle);
