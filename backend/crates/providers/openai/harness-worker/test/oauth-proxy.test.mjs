import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import http from "node:http";
import https from "node:https";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import tls from "node:tls";
import test from "node:test";
import { installFetch } from "../oauth-proxy-fetch.mjs";

const script = fileURLToPath(import.meta.url);

if (process.argv[2] === "--child") {
  installFetch({ proxyUrl: process.argv[4] });
  const response = await fetch(process.argv[3], { method: "POST", body: "grant_type=refresh_token" });
  assert.equal(response.status, 200);
  assert.deepEqual(await response.json(), { ok: true });
} else {
  const listen = (server) => new Promise((resolve) => server.listen(0, "localhost", () => resolve(server.address().port)));
  const close = (server) => new Promise((resolve) => server.close(resolve));
  const run = (url, proxy, extraEnv = {}) => new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [script, "--child", url, proxy], {
      env: { ...process.env, ...extraEnv },
      stdio: ["ignore", "pipe", "pipe"],
    });
    let errors = "";
    child.stderr.on("data", (bytes) => { errors += bytes.toString(); });
    const timer = setTimeout(() => child.kill(), 8000);
    child.on("error", reject);
    child.on("exit", (code) => {
      clearTimeout(timer);
      code === 0 ? resolve() : reject(new Error(`proxy call exited ${code}: ${errors}`));
    });
  });

  test("SOCKS5 coalesced reply keeps address bytes for the next read", async () => {
    const origin = http.createServer((request, response) => {
      request.resume();
      request.on("end", () => {
        response.writeHead(200, { "content-type": "application/json" });
        response.end('{"ok":true}');
      });
    });
    const originPort = await listen(origin);
    let connected = false;
    const proxy = net.createServer((socket) => {
      let buffer = Buffer.alloc(0);
      let stage = 0;
      const handshake = (bytes) => {
        buffer = Buffer.concat([buffer, bytes]);
        if (stage === 0 && buffer.length >= 3) {
          assert.deepEqual([...buffer.subarray(0, 3)], [5, 1, 0]);
          buffer = buffer.subarray(3);
          stage = 1;
          socket.write(Buffer.from([5, 0]));
        }
        if (stage === 1 && buffer.length >= 5 + buffer[4] + 2) {
          assert.equal(buffer[3], 3);
          const remaining = buffer.subarray(5 + buffer[4] + 2);
          stage = 2;
          socket.off("data", handshake);
          const upstream = net.connect(originPort, "localhost", () => {
            connected = true;
            // A normal SOCKS reply can arrive in one TCP packet, not one read per field.
            socket.write(Buffer.from([5, 0, 0, 1, 127, 0, 0, 1, originPort >> 8, originPort & 255]));
            if (remaining.length) upstream.write(remaining);
            socket.pipe(upstream).pipe(socket);
          });
          upstream.on("error", () => socket.destroy());
        }
      };
      socket.on("data", handshake);
    });
    const proxyPort = await listen(proxy);
    try {
      await run(`http://localhost:${originPort}/token`, `socks5h://localhost:${proxyPort}`);
      assert.equal(connected, true);
    } finally {
      await close(proxy);
      await close(origin);
    }
  });

  test("HTTPS proxy authenticates inside TLS before CONNECT", async () => {
    const dir = mkdtempSync(path.join(tmpdir(), "cpr-oauth-proxy-"));
    const key = path.join(dir, "key.pem");
    const cert = path.join(dir, "cert.pem");
    execFileSync("openssl", ["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", key, "-out", cert, "-days", "1", "-subj", "/CN=localhost", "-addext", "subjectAltName=DNS:localhost", "-addext", "basicConstraints=critical,CA:TRUE"], { stdio: "ignore" });
    const credentials = { key: readFileSync(key), cert: readFileSync(cert) };
    const origin = https.createServer(credentials, (request, response) => {
      request.resume();
      request.on("end", () => {
        response.writeHead(200, { "content-type": "application/json" });
        response.end('{"ok":true}');
      });
    });
    const originPort = await listen(origin);
    let tunnelAuthorized = false;
    const proxy = tls.createServer(credentials, (socket) => {
      socket.once("data", (bytes) => {
        const request = bytes.toString();
        assert.match(request, new RegExp(`^CONNECT localhost:${originPort} HTTP/1\\.1`));
        tunnelAuthorized = request.includes(`Proxy-Authorization: Basic ${Buffer.from("user:secret").toString("base64")}`);
        const upstream = net.connect(originPort, "localhost", () => {
          socket.write("HTTP/1.1 200 Connection Established\r\n\r\n");
          socket.pipe(upstream).pipe(socket);
        });
        upstream.on("error", () => socket.destroy());
      });
    });
    const proxyPort = await listen(proxy);
    try {
      await run(`https://localhost:${originPort}/token`, `https://user:secret@localhost:${proxyPort}`, { NODE_EXTRA_CA_CERTS: cert });
      assert.equal(tunnelAuthorized, true);
    } finally {
      await close(proxy);
      await close(origin);
      rmSync(dir, { recursive: true, force: true });
    }
  });
}
