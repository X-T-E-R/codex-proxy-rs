// 在隔离的发行目录用 mock 验证固定源码的 OAuth 与 Responses 执行路径
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import readline from "node:readline";
import { pathToFileURL } from "node:url";

const root = path.resolve(process.argv[2]);
const node = path.resolve(process.argv[3] || process.execPath);
const worker = path.join(root, "worker");
const env = {
  ...process.env,
  CPR_OPENCODE_SOURCE: path.join(root, "sources", "opencode"),
  CPR_PI_SOURCE: path.join(root, "sources", "pi"),
  CPR_HARNESS_OPENCODE_ROOT: path.join(root, "sources", "opencode"),
  CPR_HARNESS_PI_ROOT: path.join(root, "sources", "pi"),
  CPR_HARNESS_WORKER_DIR: worker,
};
const protocol = await import(pathToFileURL(path.join(worker, "protocol.mjs")));
const mock = spawn(node, [path.join(worker, "oauth-mock-upstream.mjs")], { stdio: ["ignore", "pipe", "pipe"] });
const lines = readline.createInterface({ input: mock.stdout });
const address = await new Promise((resolve, reject) => {
  lines.once("line", (line) => resolve(JSON.parse(line)));
  mock.once("error", reject);
  mock.once("exit", (code) => reject(new Error(`mock origin exited ${code}`)));
});
const origin = `http://127.0.0.1:${address.origin}`;
const scratch = mkdtempSync(path.join(tmpdir(), "cpr-package-smoke-"));

function oauth(input) {
  const result = spawnSync(node, [path.join(worker, "oauth-run.mjs")], {
    env,
    input: JSON.stringify({ ...input, upstreamOrigin: origin }),
    encoding: "utf8",
    timeout: 30000,
  });
  assert.equal(result.status, 0, `${input.harness} ${input.op}: ${result.stdout} ${result.stderr}`);
  return JSON.parse(result.stdout);
}

async function oauthSmoke(harness) {
  const authorized = oauth({ harness, op: "authorize", hostId: "test-host", hostUuid: "12345678-1234-1234-1234-123456789abc", redirectUri: "http://127.0.0.1:1455/auth/callback" });
  assert.ok(new URL(authorized.url).searchParams.get("code_challenge"));
  const clientId = `${harness}-test-client`;
  const code = `${harness}-test-code`;
  const callback = new URL(authorized.redirectUri);
  callback.searchParams.set("code", code);
  callback.searchParams.set("state", authorized.state);
  callback.searchParams.set("client_id", clientId);
  const expected = {
    clientId,
    code,
    redirectUri: authorized.redirectUri,
    nonce: authorized.nonce,
    refreshToken: `${harness}-refresh`,
    accessToken: `${harness}-access`,
  };
  const response = await fetch(`${origin}/expect`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(expected) });
  assert.equal(response.status, 204);
  const exchanged = oauth({ harness, op: "exchange", callbackUrl: callback.href, expectedState: authorized.state, verifier: authorized.verifier, nonce: authorized.nonce, redirectUri: authorized.redirectUri, jwksUrl: `${origin}/.well-known/jwks.json` });
  assert.equal(exchanged.accessToken, expected.accessToken);
  assert.equal(exchanged.clientId, clientId);
  const refreshed = oauth({ harness, op: "refresh", refreshToken: exchanged.refreshToken, accessToken: exchanged.accessToken, expiresAt: exchanged.expiresAt, clientId, scopes: exchanged.scopes });
  assert.equal(refreshed.accessToken, `${expected.accessToken}-next`);
}

async function responsesSmoke(harness) {
  const logs = path.join(scratch, `${harness}.log`);
  const child = spawn(node, ["--experimental-transform-types", "--import", pathToFileURL(path.join(worker, "register.mjs")).href, path.join(worker, "worker.mjs")], {
    env: { ...env, CPR_HARNESS_LOAD_LOG: logs },
    stdio: ["pipe", "pipe", "pipe"],
  });
  let errors = "";
  child.stderr.setEncoding("utf8").on("data", (chunk) => { errors += chunk; });
  const reader = protocol.createReader(child.stdout);
  const watchdog = setTimeout(() => child.kill(), 45000);
  const frame = (type, id, value) => child.stdin.write(protocol.encodeJson(type, id, value));
  const item = (argumentsValue) => ({ id: "fc_item", type: "function_call", call_id: "call_bridge", name: "lookup", arguments: argumentsValue });
  const events = [
    { type: "response.created", response: { id: "resp_bridge", status: "in_progress", output: [] } },
    { type: "response.output_item.added", output_index: 0, item: item("") },
    { type: "response.function_call_arguments.delta", output_index: 0, item_id: "fc_item", delta: "{\"q\":\"x\"}" },
    { type: "response.function_call_arguments.done", output_index: 0, item_id: "fc_item", arguments: "{\"q\":\"x\"}" },
    { type: "response.output_item.done", output_index: 0, item: item("{\"q\":\"x\"}") },
    { type: "response.completed", response: { id: "resp_bridge", status: "completed", output: [item("{\"q\":\"x\"}")], usage: { input_tokens: 8, output_tokens: 4, total_tokens: 12 } } },
  ];
  const body = Buffer.from(events.map((event) => `data: ${JSON.stringify(event)}\n\n`).join("") + "data: [DONE]\n\n");
  try {
    const hello = await Promise.race([reader.read(), new Promise((_, reject) => setTimeout(() => reject(new Error(errors || "worker HELLO timeout")), 15000))]);
    assert.ok(hello, `${harness} worker exited before HELLO: ${errors}`);
    assert.equal(hello.type, protocol.HELLO, errors);
    const loaded = JSON.parse(hello.payload);
    assert.ok(loaded.sources.length >= 158, `${harness} loaded ${loaded.sources.length} sources`);
    frame(protocol.READY, 0, {});
    frame(protocol.CALL, 7, { harness, model: "gpt-4.1-mini", base_url: "https://api.openai.com/v1", bearer: `${harness}-access`, session_id: "ses_smoke", request_id: "req_smoke", prompt: "Say hello.", instructions: "Be concise.", max_output_tokens: 20, temperature: 0, event_window: 64 });
    let gotRequest = false;
    let gotTool = false;
    let gotDone = false;
    const projected = [];
    for (;;) {
      const next = await reader.read();
      assert.ok(next, `${harness} worker exited early: ${errors}`);
      if (next.type === protocol.HTTP_REQUEST) {
        const request = JSON.parse(next.payload);
        assert.equal(request.call_id, 7);
        assert.equal(request.headers.find(([key]) => key.toLowerCase() === "authorization")?.[1], `Bearer ${harness}-access`);
        gotRequest = true;
        frame(protocol.HTTP_HEAD, next.id, { status: 200, headers: [["content-type", "text/event-stream"]] });
        child.stdin.write(protocol.encodeFrame(protocol.HTTP_BODY, next.id, Buffer.concat([Buffer.from([1]), body])));
      } else if (next.type === protocol.EVENT) {
        const event = JSON.parse(next.payload);
        projected.push({ kind: event.kind, type: event.type, id: event.id, response_id: event.response_id });
        if (event.kind === "tool" && event.id === (harness === "pi" ? "call_bridge|fc_item" : "call_bridge")) gotTool = true;
        if (event.kind === "done" && event.response_id === "resp_bridge") gotDone = true;
      } else if (next.type === protocol.RESULT || next.type === protocol.FAILURE) {
        assert.equal(next.type, protocol.RESULT, `${harness} ${next.payload.toString()} ${errors}`);
        break;
      }
    }
    assert.ok(gotRequest && gotTool && gotDone, `${harness} Responses missing request/tool/done: ${JSON.stringify({ gotRequest, gotTool, gotDone, projected })}`);
  } finally {
    clearTimeout(watchdog);
    child.stdin.end();
    child.kill();
  }
}

try {
  for (const harness of ["opencode", "pi"]) {
    await oauthSmoke(harness);
    if (process.argv[4] !== "--oauth-only") await responsesSmoke(harness);
  }
  console.log(`Packaged OpenCode/pi OAuth${process.argv[4] === "--oauth-only" ? "" : " and Responses"} mock passed`);
} finally {
  mock.kill();
  rmSync(scratch, { recursive: true, force: true });
}
