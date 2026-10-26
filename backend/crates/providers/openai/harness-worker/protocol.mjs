import { Buffer } from "node:buffer";

export const HELLO = 1;
export const READY = 2;
export const CALL = 3;
export const HTTP_REQUEST = 4;
export const HTTP_HEAD = 5;
export const HTTP_BODY = 6;
export const CREDIT = 7;
export const EVENT = 8;
export const RESULT = 9;
export const CANCEL = 10;
export const FAILURE = 11;
export const EVENT_CREDIT = 12;
export const EVENT_WAIT = 13;

const MAX_FRAME = 8 * 1024 * 1024;

export function encodeFrame(type, id, payload) {
  const body = Buffer.isBuffer(payload) ? payload : Buffer.from(payload);
  const frame = Buffer.alloc(4 + 1 + 4 + body.length);
  frame.writeUInt32BE(1 + 4 + body.length, 0);
  frame.writeUInt8(type, 4);
  frame.writeUInt32BE(id >>> 0, 5);
  body.copy(frame, 9);
  if (frame.length > 4 + MAX_FRAME) throw new Error("harness frame exceeds limit");
  return frame;
}

export function encodeJson(type, id, value) {
  return encodeFrame(type, id, Buffer.from(JSON.stringify(value)));
}

export function createReader(stream) {
  let pending = Buffer.alloc(0);
  const waiters = [];
  let failed = null;
  let ended = false;

  const pump = () => {
    while (waiters.length > 0 && (failed || ended || pending.length >= 4)) {
      if (failed) {
        waiters.shift().reject(failed);
        continue;
      }
      if (pending.length < 4) {
        if (ended) waiters.shift().resolve(null);
        break;
      }
      const length = pending.readUInt32BE(0);
      if (length > MAX_FRAME) {
        failed = new Error("harness frame exceeds limit");
        waiters.shift().reject(failed);
        continue;
      }
      if (pending.length < 4 + length) {
        if (ended) {
          failed = new Error("harness frame truncated");
          waiters.shift().reject(failed);
        }
        break;
      }
      const frame = pending.subarray(4, 4 + length);
      pending = pending.subarray(4 + length);
      const type = frame.readUInt8(0);
      const id = frame.readUInt32BE(1);
      waiters.shift().resolve({ type, id, payload: Buffer.from(frame.subarray(5)) });
    }
  };

  stream.on("data", (chunk) => {
    pending = Buffer.concat([pending, chunk]);
    pump();
  });
  stream.on("end", () => {
    ended = true;
    pump();
  });
  stream.on("error", (error) => {
    failed = error;
    pump();
  });
  stream.resume();

  return {
    read() {
      return new Promise((resolve, reject) => {
        waiters.push({ resolve, reject });
        pump();
      });
    },
  };
}

let writeChain = Promise.resolve();

export function writeFrame(type, id, payload) {
  const frame = encodeFrame(type, id, payload);
  writeChain = writeChain.then(
    () =>
      new Promise((resolve, reject) => {
        process.stdout.write(frame, (error) => (error ? reject(error) : resolve()));
      }),
  );
  return writeChain;
}

export function writeJson(type, id, value) {
  return writeFrame(type, id, Buffer.from(JSON.stringify(value)));
}

export function jsonPayload(payload) {
  return JSON.parse(payload.toString("utf8"));
}
