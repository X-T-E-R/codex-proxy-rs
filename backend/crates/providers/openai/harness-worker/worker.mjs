import { createHash } from "node:crypto";
import { readFileSync, realpathSync } from "node:fs";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { deliverHttp, failHttp } from "./managed-fetch.mjs";
import { runOpenCode } from "./opencode.mjs";
import { runPi } from "./pi.mjs";
import {
  CALL,
  CANCEL,
  EVENT,
  EVENT_CREDIT,
  EVENT_WAIT,
  FAILURE,
  HELLO,
  HTTP_BODY,
  HTTP_HEAD,
  READY,
  RESULT,
  createReader,
  jsonPayload,
  writeJson,
} from "./protocol.mjs";

const opencodeRoot = process.env.CPR_HARNESS_OPENCODE_ROOT;
const piRoot = process.env.CPR_HARNESS_PI_ROOT;
const minimum = [22, 19, 0];

function versionAtLeast(actual, required) {
  const parts = actual.split(".").map((part) => Number(part));
  for (let index = 0; index < required.length; index += 1) {
    const left = parts[index] ?? 0;
    const right = required[index];
    if (left > right) return true;
    if (left < right) return false;
  }
  return true;
}

function underRoot(root, file) {
  if (!root || !file) return false;
  const prefix = resolve(root).toLowerCase();
  const candidate = file.toLowerCase();
  return candidate === prefix || candidate.startsWith(`${prefix}\\`) || candidate.startsWith(`${prefix}/`);
}

function digest(file) {
  const path = realpathSync(file);
  const sha256 = createHash("sha256").update(readFileSync(path)).digest("hex");
  return { path, sha256 };
}

function redact(secret, error) {
  const text = String(error?.stack || error?.message || error);
  return secret ? text.split(secret).join("[redacted]") : text;
}

const preload = [
  ["opencode", "packages/ai/src/protocols/openai-responses.ts"],
  ["opencode", "packages/ai/src/llm.ts"],
  ["opencode", "packages/ai/src/route/client.ts"],
  ["opencode", "packages/ai/src/route/executor.ts"],
  ["opencode", "packages/ai/src/route/auth.ts"],
  ["pi", "packages/ai/src/api/openai-responses.ts"],
  ["pi", "packages/ai/src/api/openai-responses-shared.ts"],
];
for (const [rootName, relative] of preload) {
  const root = rootName === "opencode" ? opencodeRoot : piRoot;
  await import(pathToFileURL(resolve(root, relative)).href);
}

function loadedSources() {
  const log = process.env.CPR_HARNESS_LOAD_LOG;
  const seen = new Set();
  const sources = [];
  if (!log) return sources;
  for (const line of readFileSync(log, "utf8").split(/\r?\n/)) {
    if (!line || seen.has(line)) continue;
    seen.add(line);
    const opencode = underRoot(opencodeRoot, line);
    const pi = underRoot(piRoot, line);
    if (!opencode && !pi) continue;
    const rootName = opencode ? "opencode" : "pi";
    const root = opencode ? opencodeRoot : piRoot;
    const relative = line
      .slice(resolve(root).length)
      .replace(/^[/\\]/, "")
      .replaceAll("\\", "/");
    sources.push({ id: `${rootName}:${relative}`, ...digest(line) });
  }
  return sources;
}

const sources = loadedSources();

if (!versionAtLeast(process.versions.node, minimum)) {
  process.stderr.write("nodejs version is below 22.19.0\n");
  process.exit(1);
}

await writeJson(HELLO, 0, {
  node: process.versions.node,
  sources: sources.map((source) => ({ id: source.id, path: source.path, sha256: source.sha256 })),
});

const reader = createReader(process.stdin);
const calls = new Map();

class EventWindow {
  constructor(capacity) {
    this.remaining = capacity;
    this.waiters = [];
  }

  credit(slots) {
    const count = Number.isFinite(slots) && slots > 0 ? Math.floor(slots) : 1;
    this.remaining += count;
    while (this.remaining > 0 && this.waiters.length > 0) {
      this.remaining -= 1;
      this.waiters.shift().resolve();
    }
  }

  async acquire(signal, callId) {
    if (signal?.aborted) throw new Error("cancelled");
    if (this.remaining > 0) {
      this.remaining -= 1;
      return;
    }
    await writeJson(EVENT_WAIT, callId, {});
    if (signal?.aborted) throw new Error("cancelled");
    if (this.remaining > 0) {
      this.remaining -= 1;
      return;
    }
    await new Promise((resolve, reject) => {
      const waiter = { resolve, reject };
      this.waiters.push(waiter);
      const onAbort = () => {
        this.waiters = this.waiters.filter((item) => item !== waiter);
        reject(new Error("cancelled"));
      };
      if (signal?.aborted) {
        onAbort();
        return;
      }
      signal?.addEventListener("abort", onAbort, { once: true });
    });
  }
}

for (;;) {
  const frame = await reader.read();
  if (frame == null) break;
  if (frame.type === READY) continue;
  if (frame.type === HTTP_HEAD || frame.type === HTTP_BODY) {
    deliverHttp(frame);
    continue;
  }
  if (frame.type === CANCEL) {
    const call = calls.get(frame.id);
    call?.controller.abort();
    failHttp(frame.id);
    continue;
  }
  if (frame.type === EVENT_CREDIT) {
    const credit = jsonPayload(frame.payload);
    calls.get(frame.id)?.gate.credit(credit.slots);
    continue;
  }
  if (frame.type !== CALL) continue;
  if (calls.has(frame.id)) {
    await writeJson(FAILURE, frame.id, { cancelled: false, message: "call id is already active" });
    continue;
  }
  const payload = jsonPayload(frame.payload);
  const controller = new AbortController();
  const window = Number(payload.event_window);
  const gate = new EventWindow(Number.isFinite(window) && window > 0 ? Math.floor(window) : 1);
  const secret = typeof payload.bearer === "string" ? payload.bearer : "";
  calls.set(frame.id, { controller, gate, secret });
  handleCall(frame.id, payload, controller, gate, secret).finally(() => {
    if (calls.get(frame.id)?.controller === controller) calls.delete(frame.id);
  });
}

async function handleCall(id, payload, controller, gate, secret) {
  let events = 0;
  const emit = async (event) => {
    await gate.acquire(controller.signal, id);
    events += 1;
    await writeJson(EVENT, id, event);
  };
  try {
    if (payload.harness === "opencode") await runOpenCode(payload, emit, controller.signal, id);
    else if (payload.harness === "pi") await runPi(payload, emit, controller.signal, id);
    else throw new Error("unknown harness");
    if (controller.signal.aborted) {
      await writeJson(FAILURE, id, { cancelled: true, message: "cancelled" });
      return;
    }
    await writeJson(RESULT, id, { ok: true, events });
  } catch (error) {
    const cancelled = controller.signal.aborted || String(error?.message || error) === "cancelled";
    await writeJson(FAILURE, id, {
      cancelled,
      message: cancelled ? "cancelled" : redact(secret, error).slice(0, 1500),
    });
  }
}
