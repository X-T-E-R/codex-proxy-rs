// pi 在缺少 apiKey 时把这两个头换成占位 unused，从而走错 ChatGPT 分支
const CONFLICTING = new Set(["authorization", "cf-aig-authorization"]);

export function downstreamHeaders(entries) {
  const headers = {};
  if (!Array.isArray(entries)) return headers;
  for (const entry of entries) {
    if (!Array.isArray(entry) || entry.length < 2) continue;
    const name = String(entry[0]);
    if (CONFLICTING.has(name.toLowerCase())) continue;
    headers[name] = String(entry[1]);
  }
  return headers;
}
