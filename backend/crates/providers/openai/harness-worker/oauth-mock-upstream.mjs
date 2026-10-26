import { createSign, generateKeyPairSync } from "node:crypto";
import http from "node:http";

const { privateKey, publicKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });
const jwk = publicKey.export({ format: "jwk" });
jwk.alg = "RS256";
jwk.use = "sig";
jwk.kid = "cpr-test";

const clients = new Map();
const recorded = [];

function signJwt(payload) {
  const header = Buffer.from(JSON.stringify({ alg: "RS256", kid: "cpr-test", typ: "JWT" })).toString("base64url");
  const body = Buffer.from(JSON.stringify(payload)).toString("base64url");
  const data = `${header}.${body}`;
  const signature = createSign("RSA-SHA256").update(data).end().sign(privateKey).toString("base64url");
  return `${data}.${signature}`;
}

function readBody(request) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    request.on("data", (chunk) => chunks.push(chunk));
    request.on("end", () => resolve(Buffer.concat(chunks).toString("utf8")));
    request.on("error", reject);
  });
}

function send(response, status, value, headers = {}) {
  const body = typeof value === "string" ? value : JSON.stringify(value);
  response.writeHead(status, { "content-type": "application/json", ...headers });
  response.end(body);
}

async function handleOrigin(request, response) {
  const url = new URL(request.url ?? "/", "http://127.0.0.1");
  if (request.method === "GET" && url.pathname === "/.well-known/jwks.json") {
    send(response, 200, { keys: [jwk] });
    return;
  }
  if (request.method === "GET" && url.pathname === "/recorded") {
    send(response, 200, recorded);
    return;
  }
  if (request.method === "POST" && url.pathname === "/expect") {
    const client = JSON.parse(await readBody(request));
    clients.set(client.clientId, client);
    send(response, 204, "");
    return;
  }
  if (request.method === "POST" && url.pathname === "/api/accounts/oauth/token") {
    const raw = await readBody(request);
    const params = new URLSearchParams(raw);
    const clientId = params.get("client_id") ?? "";
    recorded.push({
      clientId,
      grant: params.get("grant_type"),
      refreshToken: params.get("refresh_token"),
      code: params.get("code"),
      redirect: params.get("redirect_uri"),
      resource: params.get("resource"),
      proxy: request.headers["x-cpr-test-proxy"] ?? "",
      body: raw,
    });
    const client = clients.get(clientId);
    if (!client) {
      send(response, 400, { error: "unknown_client" });
      return;
    }
    if (params.get("grant_type") === "refresh_token") {
      if (params.get("refresh_token") !== client.refreshToken) {
        send(response, 400, { error: "invalid_grant" });
        return;
      }
      client.refreshToken = client.nextRefresh ?? `${client.refreshToken}-next`;
      client.accessToken = client.nextAccess ?? `${client.accessToken}-next`;
      send(response, 200, tokenBody(client));
      return;
    }
    if (params.get("code") !== client.code || params.get("redirect_uri") !== client.redirectUri || !params.get("code_verifier")) {
      send(response, 400, { error: "invalid_grant" });
      return;
    }
    send(response, 200, tokenBody(client));
    return;
  }
  send(response, 404, { error: "not_found" });
}

function tokenBody(client) {
  const now = Math.floor(Date.now() / 1000);
  return {
    access_token: client.accessToken,
    refresh_token: client.refreshToken,
    expires_in: 3600,
    id_token: signJwt({
      iss: "https://auth.openai.com",
      aud: client.clientId,
      exp: now + 3600,
      nonce: client.nonce,
      sub: "subject-independent-of-account",
    }),
    scope: "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct",
  };
}

function listen(server) {
  return new Promise((resolve) => {
    server.listen(0, "127.0.0.1", () => resolve(server.address().port));
  });
}

const origin = http.createServer((request, response) => {
  handleOrigin(request, response).catch(() => send(response, 500, { error: "mock_failed" }));
});
const originPort = await listen(origin);
const proxy = http.createServer((request, response) => {
  const target = new URL(request.url ?? "/", "http://127.0.0.1");
  const headers = { ...request.headers, host: target.host, "x-cpr-test-proxy": "1" };
  const forward = http.request(
    {
      hostname: target.hostname,
      port: target.port,
      path: `${target.pathname}${target.search}`,
      method: request.method,
      headers,
    },
    (upstream) => {
      response.writeHead(upstream.statusCode ?? 502, upstream.headers);
      upstream.pipe(response);
    },
  );
  forward.on("error", () => send(response, 502, { error: "proxy_failed" }));
  request.pipe(forward);
});
const proxyPort = await listen(proxy);
process.stdout.write(`${JSON.stringify({ origin: originPort, proxy: proxyPort })}\n`);
