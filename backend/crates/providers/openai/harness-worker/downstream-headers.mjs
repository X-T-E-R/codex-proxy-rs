// 上游头只由两份源库和选定账号生成
// pi 在自己的会话头和 User-Agent 之后合并 options.headers，下游同名头会覆盖源码
// 身份、会话、传输和 hop-by-hop 不进入源库
// 允许透传的名字必须同时是非身份扩展，且源码本来就要发给 OpenAI；当前没有这样的头
const ALLOWED = new Set();

const DOWNSTREAM_OWNED = new Set([
  "authorization",
  "proxy-authorization",
  "cf-aig-authorization",
  "x-api-key",
  "api-key",
  "cookie",
  "cookie2",
  "chatgpt-account-id",
  "chatgpt-project-id",
  "openai-organization",
  "openai-project",
  "originator",
  "version",
  "user-agent",
  "session_id",
  "session-id",
  "x-session-id",
  "thread-id",
  "x-thread-id",
  "x-client-request-id",
  "x-request-id",
  "accept",
  "accept-encoding",
  "content-type",
  "content-length",
  "host",
  "connection",
  "transfer-encoding",
  "keep-alive",
  "upgrade",
  "proxy-connection",
  "te",
  "trailer",
  "forwarded",
  "via",
  "x-real-ip",
  "x-forwarded-for",
  "x-forwarded-host",
  "x-forwarded-proto",
]);

function downstreamOwned(name) {
  const lower = name.toLowerCase();
  return (
    DOWNSTREAM_OWNED.has(lower) ||
    lower.startsWith("x-codex-") ||
    lower.startsWith("x-openai-") ||
    lower.startsWith("x-oai-") ||
    lower.startsWith("openai-")
  );
}

export function downstreamHeaders(entries) {
  const headers = {};
  if (!Array.isArray(entries)) return headers;
  for (const entry of entries) {
    if (!Array.isArray(entry) || entry.length < 2) continue;
    const name = String(entry[0]);
    const lower = name.toLowerCase();
    if (!ALLOWED.has(lower) || downstreamOwned(name)) continue;
    headers[name] = String(entry[1]);
  }
  return headers;
}
