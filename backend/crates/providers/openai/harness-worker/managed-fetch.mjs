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
    channel.body(bytes, flags);
  }
}

export function failHttp(callId) {
  for (const [id, channel] of channels) {
    if (callId == null || channel.callId === callId) {
      channel.fail(new Error("harness call cancelled"));
      channels.delete(id);
    }
  }
}

function takeId() {
  const id = nextId;
  nextId = nextId === 0xffffffff ? 1 : nextId + 1;
  return id;
}

function openChannel(id, callId) {
  let headResolve;
  let headReject;
  let settled = false;
  const head = new Promise((resolve, reject) => {
    headResolve = resolve;
    headReject = reject;
  });
  const queue = [];
  let finished = false;
  let failure = null;
  let waiter = null;
  const finishWait = (error) => {
    finished = true;
    if (!waiter) return;
    const current = waiter;
    waiter = null;
    if (error) current.reject(error);
    else current.resolve({ bytes: Buffer.alloc(0), fin: true, failed: false });
  };
  const fail = (error) => {
    if (settled) return;
    settled = true;
    failure = error;
    channels.delete(id);
    if (headResolve) {
      headReject(error);
      headResolve = null;
    }
    finishWait(error);
  };
  const channel = {
    callId,
    head: (value) => {
      if (!headResolve) return;
      if (!value.status) {
        fail(new Error(value.error || "managed dial failed"));
        return;
      }
      headResolve(value);
      headResolve = null;
    },
    body: (bytes, flags) => {
      const fin = (flags & 1) === 1;
      const failed = (flags & 2) === 2;
      if (failed) {
        fail(new Error("managed stream failed"));
        return;
      }
      const chunk = { bytes: Buffer.from(bytes), fin, failed: false };
      if (waiter) {
        const current = waiter;
        waiter = null;
        current.resolve(chunk);
      } else {
        queue.push(chunk);
      }
      if (fin) {
        settled = true;
        channels.delete(id);
        finished = true;
      }
    },
    fail,
  };
  channels.set(id, channel);
  return {
    head,
    fail,
    async pull() {
      if (queue.length > 0) return queue.shift();
      if (failure) throw failure;
      if (finished) return { bytes: Buffer.alloc(0), fin: true, failed: false };
      return new Promise((resolve, reject) => {
        waiter = { resolve, reject };
      });
    },
    close() {
      settled = true;
      channels.delete(id);
      finishWait(null);
    },
  };
}

async function exchange(method, url, headers, body, signal, callId) {
  const id = takeId();
  const channel = openChannel(id, callId);
  let detached = false;
  const detach = () => {
    if (detached) return;
    detached = true;
    signal?.removeEventListener("abort", abort);
  };
  const abort = () => {
    try {
      channel.fail(new Error("harness call cancelled"));
      channel.close();
    } catch {
      // AbortSignal 会把监听器里的异常重新抛出并结束进程
    } finally {
      detach();
    }
  };
  signal?.addEventListener("abort", abort);
  try {
    await writeJson(HTTP_REQUEST, id, {
      method,
      url,
      headers,
      body_base64: Buffer.from(body).toString("base64"),
      call_id: callId,
    });
    const head = await channel.head;
    const stream = new ReadableStream({
      async pull(controller) {
        const chunk = await channel.pull();
        if (chunk.bytes.length > 0) {
          controller.enqueue(new Uint8Array(chunk.bytes));
          await writeJson(CREDIT, id, { bytes: chunk.bytes.length, call_id: callId });
        }
        if (chunk.fin) {
          controller.close();
          channel.close();
          detach();
        }
      },
      cancel() {
        channel.fail(new Error("harness stream cancelled"));
        channel.close();
        detach();
      },
    });
    const responseHeaders = new Headers();
    for (const [name, value] of head.headers ?? []) responseHeaders.append(name, value);
    return new Response(stream, { status: head.status, headers: responseHeaders });
  } catch (error) {
    channel.fail(error);
    channel.close();
    detach();
    throw error;
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

export async function managedFetch(url, init = {}, callId = 0) {
  const method = init.method ?? "GET";
  const headers = headerEntries(init.headers);
  let body = new Uint8Array();
  if (init.body != null) {
    if (typeof init.body === "string") body = Buffer.from(init.body);
    else if (init.body instanceof Uint8Array) body = init.body;
    else body = new Uint8Array(await new Response(init.body).arrayBuffer());
  }
  return exchange(method, String(url), headers, body, init.signal, callId);
}
