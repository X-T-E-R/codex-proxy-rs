import net from "node:net";
import tls from "node:tls";

const nativeFetch = globalThis.fetch.bind(globalThis);

function encodeBasic(url) {
  if (!url.username && !url.password) return null;
  const user = decodeURIComponent(url.username);
  const password = decodeURIComponent(url.password);
  return `Basic ${Buffer.from(`${user}:${password}`).toString("base64")}`;
}

function rewriteUpstream(url, upstreamOrigin) {
  if (!upstreamOrigin) return url;
  const target = new URL(url);
  if (target.hostname !== "auth.openai.com") return url;
  const next = new URL(upstreamOrigin);
  next.pathname = target.pathname;
  next.search = target.search;
  return next.toString();
}

function bodyText(body) {
  if (body == null) return "";
  if (typeof body === "string") return body;
  if (body instanceof URLSearchParams) return body.toString();
  return String(body);
}

function decodeChunked(body) {
  const parts = [];
  let offset = 0;
  while (offset < body.length) {
    const lineEnd = body.indexOf("\r\n", offset);
    if (lineEnd < 0) return null;
    const size = Number.parseInt(body.subarray(offset, lineEnd).toString("utf8").split(";")[0], 16);
    if (!Number.isFinite(size)) return null;
    const start = lineEnd + 2;
    if (size === 0) return Buffer.concat(parts);
    const end = start + size;
    if (end + 2 > body.length) return null;
    parts.push(body.subarray(start, end));
    offset = end + 2;
  }
  return null;
}

function readResponse(socket) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let settled = false;
    const fail = (error) => {
      if (settled) return;
      settled = true;
      socket.destroy();
      reject(error);
    };
    const tryFinish = (force) => {
      if (settled) return;
      const raw = Buffer.concat(chunks);
      const split = raw.indexOf("\r\n\r\n");
      if (split < 0) {
        if (force) fail(new Error("upstream response headers were truncated"));
        return;
      }
      const headerText = raw.subarray(0, split).toString("utf8");
      const [statusLine, ...headerLines] = headerText.split("\r\n");
      const match = /^HTTP\/\d(?:\.\d)?\s+(\d+)\s*(.*)$/.exec(statusLine ?? "");
      if (!match) {
        if (force) fail(new Error("upstream response status was invalid"));
        return;
      }
      const headers = new Headers();
      for (const line of headerLines) {
        const index = line.indexOf(":");
        if (index > 0) headers.append(line.slice(0, index).trim(), line.slice(index + 1).trim());
      }
      const chunked = (headers.get("transfer-encoding") ?? "").toLowerCase().includes("chunked");
      headers.delete("transfer-encoding");
      headers.delete("connection");
      const encoded = raw.subarray(split + 4);
      let payload;
      if (chunked) {
        payload = decodeChunked(encoded);
        if (!payload) {
          if (force) fail(new Error("upstream response body was truncated"));
          return;
        }
        headers.delete("content-length");
      } else {
        const length = headers.get("content-length");
        if (length == null) {
          if (!force) return;
          payload = encoded;
        } else if (encoded.length < Number(length)) {
          if (force) fail(new Error("upstream response body was truncated"));
          return;
        } else {
          payload = encoded.subarray(0, Number(length));
        }
      }
      settled = true;
      socket.destroy();
      resolve(new Response(payload, { status: Number(match[1]), statusText: match[2] ?? "", headers }));
    };
    socket.on("data", (chunk) => {
      chunks.push(chunk);
      tryFinish(false);
    });
    socket.on("error", fail);
    socket.on("end", () => tryFinish(true));
  });
}

function connect(host, port) {
  return new Promise((resolve, reject) => {
    const socket = net.connect({ host, port }, () => resolve(socket));
    socket.once("error", reject);
  });
}

function writeHttp(socket, requestTarget, method, headers, body, hostHeader) {
  const requestHeaders = new Headers(headers);
  if (body && !requestHeaders.has("content-length")) requestHeaders.set("content-length", String(Buffer.byteLength(body)));
  requestHeaders.delete("host");
  requestHeaders.delete("connection");
  let text = `${method} ${requestTarget} HTTP/1.1\r\n`;
  if (hostHeader) text += `host: ${hostHeader}\r\n`;
  text += "connection: close\r\n";
  for (const [name, value] of requestHeaders) text += `${name}: ${value}\r\n`;
  text += "\r\n";
  socket.write(text);
  if (body) socket.write(body);
}

async function httpProxy(target, init, body, proxy) {
  const socket = await connect(proxy.hostname, Number(proxy.port));
  const basic = encodeBasic(proxy);
  if (target.protocol === "http:") {
    const headers = new Headers(init.headers);
    if (basic) headers.set("proxy-authorization", basic);
    writeHttp(socket, target.toString(), init.method ?? "GET", headers, body, target.host);
    return readResponse(socket);
  }
  const connectHeaders = [`Host: ${target.hostname}:${target.port || 443}`];
  if (basic) connectHeaders.push(`Proxy-Authorization: ${basic}`);
  socket.write(`CONNECT ${target.hostname}:${target.port || 443} HTTP/1.1\r\n${connectHeaders.join("\r\n")}\r\n\r\n`);
  const connected = await new Promise((resolve, reject) => {
    let buffered = Buffer.alloc(0);
    const onData = (chunk) => {
      buffered = Buffer.concat([buffered, chunk]);
      const split = buffered.indexOf("\r\n\r\n");
      if (split < 0) return;
      socket.off("data", onData);
      const status = buffered.subarray(0, split).toString("utf8");
      if (!status.includes(" 200 ")) {
        socket.destroy();
        reject(new Error("outbound proxy rejected the tunnel"));
        return;
      }
      const rest = buffered.subarray(split + 4);
      resolve(rest);
    };
    socket.on("data", onData);
    socket.once("error", reject);
  });
  const secure = tls.connect({
    socket,
    servername: target.hostname,
    rejectUnauthorized: true,
  });
  await new Promise((resolve, reject) => {
    secure.once("secureConnect", resolve);
    secure.once("error", reject);
  });
  if (connected.length > 0) secure.unshift(connected);
  writeHttp(secure, `${target.pathname}${target.search}`, init.method ?? "GET", init.headers, body, target.host);
  return readResponse(secure);
}

function socksGreeting(proxy) {
  const basic = encodeBasic(proxy);
  if (!basic) return Buffer.from([0x05, 0x01, 0x00]);
  return Buffer.from([0x05, 0x01, 0x02]);
}

function socksAuth(proxy) {
  const user = Buffer.from(decodeURIComponent(proxy.username));
  const password = Buffer.from(decodeURIComponent(proxy.password));
  return Buffer.concat([Buffer.from([0x01, user.length]), user, Buffer.from([password.length]), password]);
}

function socksConnectRequest(target) {
  const host = Buffer.from(target.hostname);
  const port = target.port ? Number(target.port) : target.protocol === "https:" ? 443 : 80;
  return Buffer.concat([
    Buffer.from([0x05, 0x01, 0x00, 0x03, host.length]),
    host,
    Buffer.from([(port >> 8) & 0xff, port & 0xff]),
  ]);
}

async function readBytes(socket, size) {
  const chunks = [];
  let got = 0;
  while (got < size) {
    const chunk = await new Promise((resolve, reject) => {
      const onData = (data) => {
        socket.off("error", onError);
        resolve(data);
      };
      const onError = (error) => {
        socket.off("data", onData);
        reject(error);
      };
      socket.once("data", onData);
      socket.once("error", onError);
    });
    chunks.push(chunk);
    got += chunk.length;
  }
  return Buffer.concat(chunks).subarray(0, size);
}

async function socksProxy(target, init, body, proxy) {
  const socket = await connect(proxy.hostname, Number(proxy.port));
  socket.write(socksGreeting(proxy));
  const greeting = await readBytes(socket, 2);
  if (greeting[0] !== 0x05) throw new Error("outbound proxy is not socks5");
  if (greeting[1] === 0x02) {
    socket.write(socksAuth(proxy));
    const auth = await readBytes(socket, 2);
    if (auth[1] !== 0x00) throw new Error("outbound proxy rejected the credentials");
  } else if (greeting[1] !== 0x00) {
    throw new Error("outbound proxy requires unsupported authentication");
  }
  socket.write(socksConnectRequest(target));
  const head = await readBytes(socket, 4);
  if (head[1] !== 0x00) throw new Error("outbound proxy rejected the connection");
  if (head[3] === 0x01) await readBytes(socket, 4 + 2);
  else if (head[3] === 0x03) {
    const length = (await readBytes(socket, 1))[0];
    await readBytes(socket, length + 2);
  } else if (head[3] === 0x04) await readBytes(socket, 16 + 2);
  let stream = socket;
  if (target.protocol === "https:") {
    stream = tls.connect({ socket, servername: target.hostname, rejectUnauthorized: true });
    await new Promise((resolve, reject) => {
      stream.once("secureConnect", resolve);
      stream.once("error", reject);
    });
  }
  writeHttp(stream, `${target.pathname}${target.search}`, init.method ?? "GET", init.headers, body, target.host);
  return readResponse(stream);
}

export function installFetch(options) {
  const proxyUrl = options.proxyUrl || "";
  const upstreamOrigin = options.upstreamOrigin || "";
  globalThis.fetch = async (url, init = {}) => {
    const target = rewriteUpstream(String(url), upstreamOrigin);
    if (!proxyUrl) return nativeFetch(target, init);
    const proxy = new URL(proxyUrl);
    const body = bodyText(init.body);
    if (proxy.protocol === "socks5:" || proxy.protocol === "socks5h:") {
      return socksProxy(new URL(target), init, body, proxy);
    }
    if (proxy.protocol === "http:" || proxy.protocol === "https:") {
      return httpProxy(new URL(target), init, body, proxy);
    }
    throw new Error("unsupported outbound proxy");
  };
}
