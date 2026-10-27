import { pathToFileURL } from "node:url";
import { resolve } from "node:path";
import { downstreamHeaders } from "./downstream-headers.mjs";
import { piInput, validateOptions } from "./input.mjs";
import { managedFetch } from "./managed-fetch.mjs";

function text(value) {
  return typeof value === "string" && value.trim() !== "" ? value.trim() : undefined;
}

function source(root, relative) {
  return pathToFileURL(resolve(root, relative)).href;
}

export async function runPi(input, emit, signal, callId) {
  const root = process.env.CPR_HARNESS_PI_ROOT;
  const responses = await import(source(root, "packages/ai/src/api/openai-responses.ts"));
  const body = input.responses_body;
  validateOptions(body, "pi");
  const transcript = body ? piInput(body, input.model) : {
    messages: [
      ...(input.instructions ? [{ role: "system", content: input.instructions, timestamp: 1 }] : []),
      { role: "user", content: input.prompt, timestamp: 2 },
    ],
    hasImage: false,
  };
  const model = {
    id: input.model,
    name: input.model,
    api: "openai-responses",
    provider: "openai",
    baseUrl: input.base_url,
    reasoning: Boolean(body?.reasoning),
    input: transcript.hasImage ? ["text", "image"] : ["text"],
    cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
    contextWindow: 128000,
    maxTokens: 4096,
  };
  const serviceTier = text(input.service_tier);
  const requestId = text(input.request_id);
  const sessionId = text(input.session_id);
  if (!sessionId || !requestId) throw new Error("missing session or request id");
  const headers = downstreamHeaders(input.downstream_headers);
  // apiKey 是账号访问令牌，不能省，也不能换成 unused 或下游 Authorization
  // 源码在 ChatGPT 省略字段之后仍写入 serviceTier；无档位时不传，避免空字符串进正文
  const stream = responses.stream(model, { messages: transcript.messages }, {
    apiKey: input.bearer,
    sessionId: text(input.prompt_cache_key) ?? sessionId,
    ...(serviceTier !== undefined ? { serviceTier } : {}),
    ...(body?.reasoning?.effort ? { reasoningEffort: body.reasoning.effort } : {}),
    ...(body?.reasoning?.summary ? { reasoningSummary: body.reasoning.summary } : {}),
    ...(body?.tool_choice != null ? { toolChoice: body.tool_choice } : {}),
    ...(body?.max_output_tokens != null || input.max_output_tokens != null ? { maxTokens: body?.max_output_tokens ?? input.max_output_tokens } : {}),
    ...(body?.temperature != null || input.temperature != null ? { temperature: body?.temperature ?? input.temperature } : {}),
    ...(body?.top_p != null || body?.parallel_tool_calls != null ? { samplingParams: {
      ...(body?.top_p != null ? { top_p: body.top_p } : {}),
      ...(body?.parallel_tool_calls != null ? { parallel_tool_calls: body.parallel_tool_calls } : {}),
    } } : {}),
    ...(Object.keys(headers).length > 0 ? { headers } : {}),
    fetch: (url, init) => managedFetch(url, init, callId, requestId, emit),
    signal,
    maxRetries: 0,
  });
  for await (const event of stream) {
    if (event.type === "toolcall_end") {
      await emit({
        kind: "tool",
        type: event.type,
        id: event.toolCall?.id ?? null,
        name: event.toolCall?.name ?? null,
        arguments: event.toolCall?.arguments,
      });
    } else if (event.type === "done") {
      const tool = event.message?.content?.find((item) => item.type === "toolCall");
      if (tool) {
        await emit({
          kind: "tool",
          type: "done-tool",
          id: tool.id ?? null,
          name: tool.name ?? null,
          arguments: tool.arguments,
        });
      }
      await emit({
        kind: "done",
        type: event.type,
        finish: event.reason ?? null,
        response_id: event.message?.responseId ?? null,
        usage: event.message?.usage,
      });
    } else if (event.type === "error") {
      throw new Error("pi responses stream failed");
    } else if (event.type) {
      await emit({ kind: "trace", type: event.type, text: event.delta ?? event.text });
    }
  }
}
