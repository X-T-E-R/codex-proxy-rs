import { register } from "node:module";

await register("./oauth-jose-loader.mjs", import.meta.url);

const { installFetch } = await import("./oauth-proxy-fetch.mjs");
const { loadHarness } = await import("./oauth-source.mjs");

const chunks = [];
for await (const chunk of process.stdin) chunks.push(chunk);
const request = JSON.parse(Buffer.concat(chunks).toString("utf8"));

let fetchState;
try {
  fetchState = installFetch({
    proxyUrl: request.proxyUrl || process.env.CPR_OAUTH_PROXY_URL || "",
    upstreamOrigin: request.upstreamOrigin || process.env.CPR_OAUTH_UPSTREAM_ORIGIN || "",
  });
  const harness = await loadHarness(request.harness);
  let result;
  if (request.op === "authorize") result = await harness.cprAuthorize(request);
  else if (request.op === "exchange") result = await harness.cprExchange(request);
  else if (request.op === "refresh") result = await harness.cprRefresh(request);
  else throw Object.assign(new Error("unknown oauth operation"), { code: "callback" });
  process.stdout.write(JSON.stringify(result));
} catch (error) {
  const message = error instanceof Error ? error.message : "oauth harness failed";
  const rawCode = error && typeof error === "object" && "code" in error ? error.code : "upstream";
  const code = request.op === "refresh" && fetchState?.invalidGrant()
    ? "invalid_grant"
    : rawCode === "callback" || rawCode === "token" || rawCode === "upstream" ? rawCode : "upstream";
  process.stdout.write(JSON.stringify({ error: { code, message: String(message).slice(0, 300) } }));
  process.exitCode = 1;
}
