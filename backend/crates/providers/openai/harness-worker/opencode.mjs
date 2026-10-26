import { pathToFileURL } from "node:url";
import { resolve } from "node:path";
import { Effect, Fiber, Layer, Stream } from "effect";
import { HttpClient, HttpClientRequest, HttpClientResponse } from "effect/http";
import { downstreamHeaders } from "./downstream-headers.mjs";
import { managedFetch } from "./managed-fetch.mjs";

function text(value) {
  return typeof value === "string" && value.trim() !== "" ? value.trim() : undefined;
}

function source(root, relative) {
  return pathToFileURL(resolve(root, relative)).href;
}

function responseId(metadata) {
  if (!metadata || typeof metadata !== "object") return null;
  for (const value of Object.values(metadata)) {
    if (value && typeof value === "object" && typeof value.responseId === "string") return value.responseId;
  }
  return null;
}

function project(event) {
  const type = event?.type;
  if (type === "tool-call" || type === "tool-input-error") {
    return {
      kind: "tool",
      type,
      id: event.id ?? null,
      name: event.name ?? null,
      arguments: event.input ?? event.arguments,
    };
  }
  if (type === "finish" || type === "step-finish") {
    return {
      kind: "done",
      type,
      finish: event.reason?.normalized ?? null,
      response_id: responseId(event.providerMetadata),
      usage: event.usage,
      text: event.text,
    };
  }
  return type ? { kind: "trace", type, text: event.delta ?? event.text } : null;
}

export async function runOpenCode(input, emit, signal, callId) {
  const root = process.env.CPR_HARNESS_OPENCODE_ROOT;
  const ai = "packages/ai/src";
  const responses = await import(source(root, `${ai}/protocols/openai-responses.ts`));
  const llm = await import(source(root, `${ai}/llm.ts`));
  const client = await import(source(root, `${ai}/route/client.ts`));
  const executor = await import(source(root, `${ai}/route/executor.ts`));
  const auth = await import(source(root, `${ai}/route/auth.ts`));
  // route 默认 transport 是 WebSocket channel；这条 harness 使用同一协议导出的 HTTP/SSE transport
  const model = responses.route
    .with({
      endpoint: { baseURL: input.base_url },
      auth: auth.Auth.bearer(input.bearer),
      transport: responses.httpTransport,
    })
    .model({ id: input.model });
  const generation = {};
  if (input.max_output_tokens != null) generation.maxTokens = input.max_output_tokens;
  if (input.temperature != null) generation.temperature = input.temperature;
  const sessionId = text(input.session_id);
  const requestId = text(input.request_id);
  const serviceTier = text(input.service_tier);
  if (!sessionId || !requestId) throw new Error("missing session or request id");
  // request id 只进入 LLMRequest.id；session-id 用 affinity，thread 和 client request 重复当前会话
  const request = llm.request({
    id: requestId,
    model,
    ...(input.instructions ? { system: input.instructions } : {}),
    prompt: input.prompt,
    ...(Object.keys(generation).length > 0 ? { generation } : {}),
    ...(serviceTier ? { providerOptions: { serviceTier } } : {}),
    http: {
      headers: {
        ...downstreamHeaders(input.downstream_headers),
        "session-id": text(input.session_affinity_id) ?? sessionId,
        "thread-id": sessionId,
        "x-client-request-id": sessionId,
      },
    },
  });
  const httpLayer = Layer.succeed(
    HttpClient.HttpClient,
    HttpClient.make((httpRequest) =>
      Effect.gen(function* () {
        const web = yield* HttpClientRequest.toWeb(httpRequest).pipe(Effect.orDie);
        const bytes = new Uint8Array(yield* Effect.promise(() => web.arrayBuffer()));
        const headers = [];
        web.headers.forEach((value, name) => {
          headers.push([name, value]);
        });
        const response = yield* Effect.promise(() =>
          managedFetch(
            web.url,
            {
              method: web.method,
              headers,
              body: bytes,
              signal,
            },
            callId,
            requestId,
          ),
        );
        return HttpClientResponse.fromWeb(httpRequest, response);
      }),
    ),
  );
  const requestExecutorLayer = executor.RequestExecutor.layer.pipe(Layer.provide(httpLayer));
  const llmClientLayer = client.LLMClient.layer.pipe(Layer.provide(requestExecutorLayer));
  const runtime = Layer.mergeAll(requestExecutorLayer, llmClientLayer);
  const program = Stream.runForEach(client.LLMClient.stream(request), (event) =>
    Effect.promise(async () => {
      const projected = project(event);
      if (projected) await emit(projected);
    }),
  ).pipe(Effect.provide(runtime));
  const fiber = Effect.runFork(program);
  const interrupt = () => {
    Effect.runFork(Fiber.interrupt(fiber));
  };
  signal?.addEventListener("abort", interrupt, { once: true });
  try {
    await Effect.runPromise(Fiber.join(fiber));
  } finally {
    signal?.removeEventListener("abort", interrupt);
  }
}
