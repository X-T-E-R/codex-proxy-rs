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

function digest(file) {
  const path = realpathSync(file);
  const sha256 = createHash("sha256").update(readFileSync(path)).digest("hex");
  return { path, sha256 };
}

let secret = "";

function redact(error) {
  const text = String(error?.stack || error?.message || error);
  return secret ? text.split(secret).join("[redacted]") : text;
}

const sources = [
  ["opencode-openai-responses", resolve(opencodeRoot, "packages/ai/src/protocols/openai-responses.ts")],
  ["pi-openai-responses", resolve(piRoot, "packages/ai/src/api/openai-responses.ts")],
  ["pi-openai-responses-shared", resolve(piRoot, "packages/ai/src/api/openai-responses-shared.ts")],
].map(([id, file]) => ({ id, ...digest(file) }));

if (!versionAtLeast(process.versions.node, minimum)) {
  process.stderr.write("nodejs version is below 22.19.0\n");
  process.exit(1);
}

await import(pathToFileURL(sources[0].path).href);
await import(pathToFileURL(sources[1].path).href);

await writeJson(HELLO, 0, {
  node: process.versions.node,
  sources: sources.map((source) => ({ id: source.id, path: source.path, sha256: source.sha256 })),
});

const reader = createReader(process.stdin);
let abort = null;

for (;;) {
  const frame = await reader.read();
  if (frame == null) break;
  if (frame.type === READY) continue;
  if (frame.type === HTTP_HEAD || frame.type === HTTP_BODY) {
    deliverHttp(frame);
    continue;
  }
  if (frame.type === CANCEL) {
    abort?.abort();
    failHttp(new Error("cancelled"));
    continue;
  }
  if (frame.type !== CALL) continue;
  const payload = jsonPayload(frame.payload);
  const controller = new AbortController();
  abort = controller;
  secret = typeof payload.bearer === "string" ? payload.bearer : "";
  handleCall(frame.id, payload, controller).finally(() => {
    secret = "";
    if (abort === controller) abort = null;
  });
}

async function handleCall(id, payload, controller) {
  const events = [];
  const emit = (event) => {
    events.push(event);
    return writeJson(EVENT, id, event);
  };
  try {
    if (payload.harness === "opencode") await runOpenCode(payload, emit, controller.signal);
    else if (payload.harness === "pi") await runPi(payload, emit, controller.signal);
    else throw new Error("unknown harness");
    if (controller.signal.aborted) {
      await writeJson(FAILURE, id, { cancelled: true, message: "cancelled" });
      return;
    }
    await writeJson(RESULT, id, { ok: true, events: events.length });
  } catch (error) {
    const cancelled = controller.signal.aborted;
    await writeJson(FAILURE, id, {
      cancelled,
      message: cancelled ? "cancelled" : redact(error).slice(0, 1500),
    });
  }
}
