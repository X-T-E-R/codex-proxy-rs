import { Buffer } from "node:buffer";
import { CREDIT, HTTP_BODY, HTTP_HEAD, HTTP_REQUEST, jsonPayload, writeJson } from "./protocol.mjs";

const channels = new Map();
let nextId = 1;

export function deliverHttp(frame) {
  const channel = channels.get(frame.id);
  if (!channel) return;
  if (frame.type === HTTP_HEAD) {
    channel.head(jsonPayload(frame.payload));
    return;
  }
  if (frame.type === HTTP_BODY) {
    const flags = frame.payload.readUInt8(0);
    const bytes = frame.payload.subarray(1);
    channel.body(bytes, (flags & 1) === 1);
  }
}

export function failHttp(error) {
  for (const channel of channels.values()) channel.fail(error);
  channels.clear();
}

function takeId() {
  const id = nextId;
  nextId = nextId === 0xffffffff ? 1 : nextId + 1;
  return id;
}

function openChannel(id) {
  let headResolve;
  let headReject;
  const head = new Promise((resolve, reject) => {
    headResolve = resolve;
    headReject = reject;
  });
  const queue = [];
  let finished = false;
  let waiter = null;
  const fail = (error) => {
    if (headResolve) {
      headReject(error);
      headResolve = null;
    }
    finished = true;
    waiter?.reject(error);
    waiter = null;
  };
  const channel = {
    head: (value) => {
      if (!headResolve) return;
      if (!value.status) {
        const error = new Error(value.error || "managed dial failed");
        headReject(error);
        headResolve = null;
        fail(error);
        return;
      }
      headResolve(value);
      headResolve = null;
    },
    body: (bytes, fin) => {
      const chunk = { bytes: Buffer.from(bytes), fin };
      if (waiter) {
        const current = waiter;
        waiter = null;
        current.resolve(chunk);
        return;
      }
      queue.push(chunk);
    },
    fail,
  };
  channels.set(id, channel);
  return {
    head,
    async pull() {
      if (queue.length > 0) return queue.shift();
      if (finished) return { bytes: Buffer.alloc(0), fin: true };
      return new Promise((resolve, reject) => {
        waiter = { resolve, reject };
      });
    },
    close() {
      channels.delete(id);
    },
  };
}

async function exchange(method, url, headers, body, signal) {
  const id = takeId();
  const channel = openChannel(id);
  const abort = () => channel.fail(new Error("harness call cancelled"));
  signal?.addEventListener("abort", abort, { once: true });
  await writeJson(HTTP_REQUEST, id, {
    method,
    url,
    headers,
    body_base64: Buffer.from(body).toString("base64"),
  });
  try {
    const head = await channel.head;
    const stream = new ReadableStream({
      async pull(controller) {
        const chunk = await channel.pull();
        if (chunk.bytes.length > 0) {
          controller.enqueue(new Uint8Array(chunk.bytes));
          await writeJson(CREDIT, id, { bytes: chunk.bytes.length });
        }
        if (chunk.fin) {
          controller.close();
          channel.close();
        }
      },
      cancel() {
        channel.close();
      },
    });
    const responseHeaders = new Headers();
    for (const [name, value] of head.headers ?? []) responseHeaders.append(name, value);
    return new Response(stream, { status: head.status, headers: responseHeaders });
  } finally {
    signal?.removeEventListener("abort", abort);
  }
}

export function headerEntries(input) {
  const headers = [];
  if (!input) return headers;
  if (Array.isArray(input)) {
    for (const entry of input) {
      if (Array.isArray(entry) && entry.length >= 2) headers.push([String(entry[0]), String(entry[1])]);
    }
    return headers;
  }
  if (typeof input.forEach === "function" && !Array.isArray(input)) {
    input.forEach((value, name) => {
      headers.push([name, value]);
    });
    return headers;
  }
  for (const [name, value] of Object.entries(input)) {
    if (value == null) continue;
    headers.push([name, Array.isArray(value) ? value.join(", ") : String(value)]);
  }
  return headers;
}

export async function managedFetch(url, init = {}) {
  const method = init.method ?? "GET";
  const headers = headerEntries(init.headers);
  let body = new Uint8Array();
  if (init.body != null) {
    if (typeof init.body === "string") body = Buffer.from(init.body);
    else if (init.body instanceof Uint8Array) body = init.body;
    else body = new Uint8Array(await new Response(init.body).arrayBuffer());
  }
  return exchange(method, String(url), headers, body, init.signal);
}
