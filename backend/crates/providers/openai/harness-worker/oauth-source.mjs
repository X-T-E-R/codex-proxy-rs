import { createHash } from "node:crypto";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const workerDir = path.dirname(fileURLToPath(import.meta.url));
const effectModule = pathToFileURL(path.join(workerDir, "node_modules/effect/dist/index.js")).href;

const OPENCODE_HASH = "27f84a6177a6e01b5a1fc15dbcd39510a1d239779034533a548162e979425064";
const PI_HASH = "ad9dbdd7ffa52fcbaff1d1be2d0ba97b643476aaf8930e6dd67147bf35c837de";
const OPENCODE_FILE = "packages/core/src/plugin/provider/chatgpt.ts";
const PI_FILE = "packages/ai/src/auth/oauth/openai-chatgpt.ts";

const DIRECT_SCOPE = "chatgpt.tokens.use.direct";

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function sourceRoot(name) {
  const env = process.env[`CPR_${name.toUpperCase()}_SOURCE`];
  if (env) return env;
  const bundled = path.resolve(workerDir, "..", "sources", name);
  if (existsSync(bundled)) return bundled;
  return path.resolve(workerDir, "../../../../../../../../sources", name, "checkout");
}

function assertHash(file, expected) {
  const bytes = readFileSync(file);
  const actual = sha256(bytes);
  if (actual !== expected) {
    throw new Error(`pinned ${path.basename(file)} hash mismatch`);
  }
  return bytes.toString("utf8");
}

function tempDir() {
  const directory = mkdtempSync(path.join(tmpdir(), "cpr-oauth-"));
  process.once("exit", () => rmSync(directory, { recursive: true, force: true }));
  return directory;
}

const OPENCODE_HEADER = `
import { Effect, Option, Schema } from ${JSON.stringify(effectModule)};
const App = { useragent: (app) => \`opencode/\${app.channel}/\${app.version}/\${app.name}\` };
const Credential = { OAuth: { make: (value) => value }, Service: {}, Event: { Switched: "switched" }, ID: { make: (value) => value } };
const Bus = { Service: {} };
const Integration = { ID: { make: (value) => value }, MethodID: { make: (value) => value } };
const Model = { ID: { make: (value) => value }, Info: { default: () => ({}) }, VariantID: { make: (value) => value } };
const Provider = { ID: { openai: "openai" }, mergeOverlay: (left, right) => ({ ...left, ...right }) };
const SessionAffinity = { get: (session) => session };
const define = (plugin) => plugin;
const configuredSettings = async () => undefined;
`;

const OPENCODE_FOOTER = `
const cprApp = { name: "OpenCode", version: "cpr", channel: "stable" };
export async function cprAuthorize(input) {
  const pkce = await generatePKCE();
  const state = randomValue();
  const nonce = randomValue();
  const url = authorizeURL(input.redirectUri, pkce, state, nonce, input.savedClientId || undefined, input.hostId);
  return { url, state, nonce, verifier: pkce.verifier, redirectUri: input.redirectUri, hostId: input.hostId };
}
export async function cprExchange(input) {
  const callback = new URL(input.callbackUrl);
  const expected = new URL(input.redirectUri);
  if (callback.origin !== expected.origin || callback.pathname !== expected.pathname) {
    throw Object.assign(new Error("OAuth redirect does not match this authorization"), { code: "callback" });
  }
  if (callback.searchParams.get("state") !== input.expectedState) {
    throw Object.assign(new Error("Invalid OAuth state"), { code: "callback" });
  }
  const returnedClientId = callback.searchParams.get("client_id") || undefined;
  if (input.savedClientId && returnedClientId && returnedClientId !== input.savedClientId) {
    throw Object.assign(new Error("ChatGPT returned a different client than this connection. Connect again."), { code: "token" });
  }
  const clientId = input.savedClientId || returnedClientId;
  if (!clientId) {
    throw Object.assign(new Error("ChatGPT sign-in did not return a client ID. Connect again."), { code: "token" });
  }
  const code = callback.searchParams.get("code");
  if (!code) throw Object.assign(new Error("Missing authorization code"), { code: "callback" });
  const tokens = await Effect.runPromise(exchange(code, clientId, input.redirectUri, { verifier: input.verifier, challenge: "" }, cprApp));
  if (!tokens.scope?.split(" ").includes(tokenSharingScope)) {
    throw Object.assign(new Error("ChatGPT sign-in finished without token sharing. Sign in again and allow token sharing, or connect OpenAI with an API key."), { code: "token" });
  }
  if (!tokens.id_token) throw Object.assign(new Error("ChatGPT sign-in did not return an ID token."), { code: "token" });
  try {
    await Effect.runPromise(verifyIDToken(tokens.id_token, clientId, input.nonce, input.jwksUrl ? new URL(input.jwksUrl) : undefined));
  } catch (error) {
    const message = error instanceof Error ? error.message : "invalid id token";
    throw Object.assign(new Error(message), { code: "token" });
  }
  const credentialValue = credential(tokens, clientId);
  return present(credentialValue.access, credentialValue.refresh, credentialValue.expires, clientId, credentialValue.metadata.scopes, tokens.id_token);
}
export async function cprRefresh(input) {
  const value = {
    type: "oauth",
    methodID,
    refresh: input.refreshToken,
    access: input.accessToken,
    expires: input.expiresAt ?? 0,
    metadata: { clientID: input.clientId, scopes: input.scopes },
  };
  const refreshed = await Effect.runPromise(refresh(value, cprApp));
  if (refreshed.metadata.clientID !== input.clientId) {
    throw Object.assign(new Error("refresh returned a different client"), { code: "token" });
  }
  return present(refreshed.access, refreshed.refresh, refreshed.expires, refreshed.metadata.clientID, refreshed.metadata.scopes, undefined);
}
function present(accessToken, refreshToken, expiresAt, clientId, scopes, idToken) {
  return { accessToken, refreshToken, expiresAt, clientId, scopes, idToken: idToken ?? null };
}
`;

const PI_FOOTER = `
export async function cprAuthorize(input) {
  const hostId = agentHostId(input.hostUuid);
  const { verifier, challenge } = await generatePKCE();
  const state = randomValue();
  const nonce = randomValue();
  const authorizationUrl = new URL(AUTHORIZE_URL);
  authorizationUrl.search = new URLSearchParams({
    client_id: DYNAMIC_CLIENT_ID,
    agent_name_hint: AGENT_NAME_HINT,
    ext_agent_host_id: hostId,
    response_type: "code",
    redirect_uri: REDIRECT_URI,
    resource: RESOURCE,
    scope: SCOPE,
    state,
    code_challenge: challenge,
    code_challenge_method: "S256",
    nonce,
  }).toString();
  return { url: authorizationUrl.toString(), state, nonce, verifier, redirectUri: REDIRECT_URI, hostId };
}
export async function cprExchange(input) {
  let parsed;
  try {
    parsed = authorizationResultFromManualInput(input.callbackUrl, input.expectedState);
  } catch (error) {
    const message = error instanceof Error ? error.message : "invalid callback";
    throw Object.assign(new Error(message), { code: "callback" });
  }
  let credentialValue;
  try {
    credentialValue = await exchangeAuthorizationCode(parsed.code, input.verifier, parsed.clientId, AbortSignal.timeout(20000));
  } catch (error) {
    const message = error instanceof Error ? error.message : "token exchange failed";
    throw Object.assign(new Error(message), { code: "token" });
  }
  return {
    accessToken: credentialValue.access,
    refreshToken: credentialValue.refresh,
    expiresAt: credentialValue.expires,
    clientId: credentialValue.clientId,
    scopes: credentialValue.scopes,
    idToken: null,
  };
}
export async function cprRefresh(input) {
  const credentialValue = await refreshAccessToken({
    type: "oauth",
    access: input.accessToken,
    refresh: input.refreshToken,
    expires: input.expiresAt ?? 0,
    clientId: input.clientId,
    scopes: input.scopes,
  }, AbortSignal.timeout(20000));
  if (credentialValue.clientId !== input.clientId) {
    throw Object.assign(new Error("refresh returned a different client"), { code: "token" });
  }
  return {
    accessToken: credentialValue.access,
    refreshToken: credentialValue.refresh,
    expiresAt: credentialValue.expires,
    clientId: credentialValue.clientId,
    scopes: credentialValue.scopes,
    idToken: null,
  };
}
`;

let opencodeModule;
let piModule;

export async function loadHarness(harness) {
  if (harness === "opencode") {
    if (!opencodeModule) opencodeModule = await import(pathToFileURL(writeOpenCode()).href);
    return opencodeModule;
  }
  if (harness === "pi") {
    if (!piModule) piModule = await import(pathToFileURL(writePi()).href);
    return piModule;
  }
  throw Object.assign(new Error("unknown oauth harness"), { code: "callback" });
}

function writeOpenCode() {
  const root = sourceRoot("opencode");
  const file = path.join(root, OPENCODE_FILE);
  const source = assertHash(file, OPENCODE_HASH);
  const body = source
    .split(/\r?\n/)
    .filter((line) => !line.startsWith("import "))
    .join("\n");
  const output = path.join(tempDir(), "chatgpt.ts");
  writeFileSync(output, `${OPENCODE_HEADER}\n${body}\n${OPENCODE_FOOTER}`);
  return output;
}

function writePi() {
  const root = sourceRoot("pi");
  const file = path.join(root, PI_FILE);
  let source = assertHash(file, PI_HASH);
  const replacements = [
    ["./pkce.ts", path.join(root, "packages/ai/src/auth/oauth/pkce.ts")],
    ["../../utils/oauth-page.ts", path.join(root, "packages/ai/src/utils/oauth-page.ts")],
    ["../../utils/provider-env.ts", path.join(root, "packages/ai/src/utils/provider-env.ts")],
  ];
  for (const [specifier, target] of replacements) {
    source = source.replaceAll(`from "${specifier}"`, `from ${JSON.stringify(pathToFileURL(target).href)}`);
  }
  const output = path.join(tempDir(), "openai-chatgpt.ts");
  writeFileSync(output, `${source}\n${PI_FOOTER}`);
  return output;
}

export function requireDirectScope(scopes) {
  if (!scopes?.includes(DIRECT_SCOPE)) {
    throw Object.assign(new Error("token sharing scope is missing"), { code: "token" });
  }
}
