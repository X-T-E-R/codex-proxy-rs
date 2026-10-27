// 将公开 Responses 历史交给两份源码自己的 canonical transcript 转换器
export class UnsupportedInput extends Error {}

export function validateOptions(body, harness) {
  if (!body) return;
  const common = new Set(["model", "input", "instructions", "tools", "tool_choice", "reasoning",
    "service_tier", "max_output_tokens", "temperature", "top_p", "parallel_tool_calls", "stream"]);
  const openCode = new Set(["store", "previous_response_id", "metadata", "include", "truncation", "max_tool_calls", "text"]);
  for (const key of Object.keys(body)) {
    if (common.has(key) || (harness === "opencode" && openCode.has(key))) continue;
    if (harness === "pi" && key === "store" && body.store === false) continue;
    throw new UnsupportedInput(`Unsupported ${harness} Responses option: ${key}`);
  }
  if (body.text && (typeof body.text !== "object" || Object.keys(body.text).some((key) => key !== "verbosity"))) {
    throw new UnsupportedInput("Unsupported OpenCode text options");
  }
  const choice = body.tool_choice;
  if (choice == null) return undefined;
  if (typeof choice === "string") {
    if (!["auto", "none", "required"].includes(choice)) throw new UnsupportedInput("Unsupported tool_choice");
    return undefined;
  }
  const declared = new Set((body.tools ?? []).filter((tool) => tool.type === "function").map((tool) => tool.name));
  if (choice.type === "function" && typeof choice.name === "string" && declared.has(choice.name)) return undefined;
  if (choice.type !== "allowed_tools" || !["auto", "none", "required"].includes(choice.mode)
      || !Array.isArray(choice.tools) || choice.tools.length === 0
      || choice.tools.some((tool) => tool.type !== "function" || !declared.has(tool.name))) {
    throw new UnsupportedInput("tool_choice must only authorize declared tools");
  }
  return { mode: choice.mode, toolNames: choice.tools.map((tool) => tool.name) };
}

function content(item, kind) {
  if (typeof item === "string") return [{ type: kind, text: item }];
  if (!Array.isArray(item)) throw new UnsupportedInput("Responses message content must be an array");
  return item;
}

function toolArguments(item) {
  try {
    return JSON.parse(item.arguments ?? "{}");
  } catch {
    throw new UnsupportedInput("Invalid function-call arguments");
  }
}

function image(part) {
  if (part.type !== "input_image" || typeof part.image_url !== "string") return null;
  const match = /^data:([^;,]+);base64,([\s\S]+)$/.exec(part.image_url);
  if (!match || !match[1].startsWith("image/")) throw new UnsupportedInput("inline image data is required");
  return { type: "image", mimeType: match[1], data: match[2] };
}

function items(body) {
  const input = body.input;
  if (typeof input === "string") return [{ type: "message", role: "user", content: [{ type: "input_text", text: input }] }];
  if (!Array.isArray(input)) throw new UnsupportedInput("Responses input must be text or an array");
  return input;
}

export function openCodeInput(body, media) {
  const messages = [];
  for (const item of items(body)) {
    if (item.type === "reasoning") {
      messages.push({ role: "assistant", content: [{ type: "reasoning", text: (item.summary ?? []).map((part) => part.text ?? "").join("\n"), providerMetadata: { openai: { itemId: item.id, reasoningEncryptedContent: item.encrypted_content } } }] });
      continue;
    }
    if (item.type === "compaction") {
      messages.push({ role: "assistant", content: [{ type: "compaction", provider: "openai", id: item.id, encrypted: item.encrypted_content }] });
      continue;
    }
    if (item.type === "function_call") {
      messages.push({ role: "assistant", content: [{ type: "tool-call", id: item.call_id, name: item.name, input: toolArguments(item), providerMetadata: { openai: { itemId: item.id } } }] });
      continue;
    }
    if (item.type === "function_call_output") {
      messages.push({ role: "tool", content: [{ type: "tool-result", id: item.call_id, name: item.name ?? "tool", result: { type: "text", value: item.output } }] });
      continue;
    }
    if (item.type !== "message") throw new UnsupportedInput(`Unsupported Responses input item: ${item.type}`);
    if (!["user", "assistant", "system", "developer"].includes(item.role)) throw new UnsupportedInput("Unsupported Responses message role");
    const parts = content(item.content, item.role === "assistant" ? "output_text" : "input_text").map((part) => {
      if (part.type === "input_text" || part.type === "output_text") return { type: "text", text: part.text, ...(item.id ? { providerMetadata: { openai: { itemId: item.id } } } : {}) };
      if (part.type === "input_image" && typeof part.image_url === "string") return { type: "media", media: media.fromDataUrl(part.image_url) };
      if (part.type === "input_file" && typeof part.file_data === "string") return { type: "media", media: media.fromDataUrl(part.file_data) };
      throw new UnsupportedInput(`Unsupported Responses content: ${part.type}`);
    });
    messages.push({ role: item.role === "developer" ? "system" : item.role, content: parts });
  }
  const tools = (body.tools ?? []).map((tool) => {
    if (tool.type !== "function") throw new UnsupportedInput(`Unsupported Responses tool: ${tool.type}`);
    return { name: tool.name, description: tool.description ?? "", inputSchema: tool.parameters ?? { type: "object", properties: {} } };
  });
  const choice = typeof body.tool_choice === "string" ? body.tool_choice : body.tool_choice?.type === "function" ? body.tool_choice.name : undefined;
  return {
    ...(typeof body.instructions === "string" ? { system: body.instructions } : {}),
    messages,
    tools,
    ...(choice ? { toolChoice: choice } : {}),
  };
}

export function piInput(body, modelId) {
  const messages = [];
  let hasImage = false;
  for (const item of items(body)) {
    if (item.type === "reasoning") {
      messages.push({ role: "assistant", api: "openai-responses", provider: "openai", model: modelId, content: [{ type: "thinking", thinking: (item.summary ?? []).map((part) => part.text ?? "").join("\n"), thinkingSignature: JSON.stringify(item) }], timestamp: 1 });
      continue;
    }
    if (item.type === "function_call") {
      messages.push({ role: "assistant", api: "openai-responses", provider: "openai", model: modelId, content: [{ type: "toolCall", id: `${item.call_id}|${item.id ?? ""}`, name: item.name, arguments: toolArguments(item) }], timestamp: 1 });
      continue;
    }
    if (item.type === "function_call_output") {
      messages.push({ role: "toolResult", toolCallId: item.call_id, toolName: item.name ?? "tool", content: [{ type: "text", text: typeof item.output === "string" ? item.output : JSON.stringify(item.output) }], isError: false, timestamp: 1 });
      continue;
    }
    if (item.type !== "message") throw new UnsupportedInput(`Unsupported Responses input item: ${item.type}`);
    if (!["user", "assistant", "system", "developer"].includes(item.role)) throw new UnsupportedInput("Unsupported Responses message role");
    const parts = content(item.content, item.role === "assistant" ? "output_text" : "input_text").map((part) => {
      if (part.type === "input_text" || part.type === "output_text") return { type: "text", text: part.text, ...(item.id ? { textSignature: item.id } : {}) };
      const converted = image(part);
      if (converted) { hasImage = true; return converted; }
      throw new UnsupportedInput(`Unsupported pi Responses content: ${part.type}`);
    });
    messages.push(item.role === "assistant"
      ? { role: "assistant", api: "openai-responses", provider: "openai", model: modelId, content: parts, timestamp: 1 }
      : { role: item.role === "developer" ? "system" : item.role, content: item.role === "system" || item.role === "developer" ? parts.map((part) => part.text).join("\n") : parts, timestamp: 1 });
  }
  if (body.instructions) messages.unshift({ role: "system", content: body.instructions, timestamp: 1 });
  if (Array.isArray(body.tools) && body.tools.length) {
    const tools = body.tools.map((tool) => {
      if (tool.type !== "function") throw new UnsupportedInput(`Unsupported pi Responses tool: ${tool.type}`);
      return { name: tool.name, description: tool.description ?? "", parameters: tool.parameters ?? { type: "object", properties: {} } };
    });
    messages.unshift({ role: "system", content: "", toolsAdded: tools, timestamp: 1 });
  }
  return { messages, hasImage };
}
