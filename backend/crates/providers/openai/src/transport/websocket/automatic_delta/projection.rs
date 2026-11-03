//! 按新增轮边界认领客户端回填，原生输出拥有历史，显式业务冲突要求重建

use std::collections::{HashMap, HashSet};

use serde_json::Value;

pub(super) struct Claim {
    pub(super) tail: Vec<Value>,
}

/// 身份、旧 input、live lease 与最新 completed owner 已由调用方核实
/// user / 已知 tool-result 是新增轮边界，不按客户端字段模板推测 item 数量
pub(super) fn boundary(
    output: &[Value],
    current: &[Value],
    previous_input: &[Value],
    native_calls: &[String],
) -> Result<Claim, &'static str> {
    let count = current
        .iter()
        .position(|item| is_message(item, "user") || ToolKind::result(item).is_some())
        .ok_or("turn_boundary_missing")?;
    if count > super::MAX_OUTPUT_ITEMS {
        return Err("output_claim_limit");
    }
    let rebuilt = &current[..count];
    let native = View::read(output).ok_or("output_conflict")?;
    let client = View::read(rebuilt).ok_or("output_conflict")?;
    // 文本分段、message 合并和已知 reasoning 的缺省不改变原生内容
    // 有意删除与正常缺省完全相同时，由现有 fork/session 合同建立新基线
    if !text_equal(&native.text, &client.text)
        || !reasoning_matches(&native.reasoning, &client.reasoning)
        || !is_subsequence(&native.opaque, &client.opaque)
        || native.calls.len() != client.calls.len()
        || !explicit_fields_match(output, rebuilt)
    {
        return Err("output_conflict");
    }
    let mut aliases = HashMap::new();
    let native_ids: HashSet<_> = native.calls.iter().map(|call| call.id).collect();
    let historical: HashSet<_> = previous_input
        .iter()
        .filter(|item| is_tool_call(item))
        .filter_map(|item| item["call_id"].as_str())
        .chain(native_calls.iter().map(String::as_str))
        .collect();
    let mut signatures = HashMap::new();
    for call in &native.calls {
        *signatures
            .entry((call.kind, call.name, call.input))
            .or_insert(0_usize) += 1;
    }
    for (old, new) in native.calls.iter().zip(&client.calls) {
        if old.kind != new.kind
            || old.name != new.name
            || old.input != new.input
            || (old.id != new.id
                && (native_ids.contains(new.id)
                    || historical.contains(new.id)
                    || signatures[&(old.kind, old.name, old.input)] != 1))
            || aliases.insert(new.id, *old).is_some()
        {
            return Err("tool_conflict");
        }
    }
    let mut tail = current[count..].to_vec();
    if !map_tail(&mut tail, aliases, historical, native_ids) {
        return Err("tool_tail_invalid");
    }
    Ok(Claim { tail })
}

fn is_message(item: &Value, role: &str) -> bool {
    item["role"] == role && item.get("type").is_none_or(|kind| kind == "message")
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ToolKind {
    Function,
    Custom,
}

impl ToolKind {
    fn call(item: &Value) -> Option<Self> {
        match item["type"].as_str()? {
            "function_call" => Some(Self::Function),
            "custom_tool_call" => Some(Self::Custom),
            _ => None,
        }
    }

    fn result(item: &Value) -> Option<Self> {
        match item["type"].as_str()? {
            "function_call_output" => Some(Self::Function),
            "custom_tool_call_output" => Some(Self::Custom),
            _ => None,
        }
    }
}

pub(super) fn is_tool_call(item: &Value) -> bool {
    ToolKind::call(item).is_some()
}

#[derive(Clone, Copy)]
struct ToolCall<'a> {
    kind: ToolKind,
    id: &'a str,
    name: &'a str,
    input: &'a str,
}

impl<'a> ToolCall<'a> {
    fn read(item: &'a Value) -> Option<Self> {
        let kind = ToolKind::call(item)?;
        Some(Self {
            kind,
            id: item["call_id"].as_str().filter(|id| !id.is_empty())?,
            name: item["name"].as_str().filter(|name| !name.is_empty())?,
            input: item[match kind {
                ToolKind::Function => "arguments",
                ToolKind::Custom => "input",
            }]
            .as_str()?,
        })
    }
}

#[derive(Default)]
struct View<'a> {
    text: Vec<&'a str>,
    reasoning: Vec<&'a Value>,
    opaque: Vec<&'a Value>,
    calls: Vec<ToolCall<'a>>,
}

impl<'a> View<'a> {
    fn read(items: &'a [Value]) -> Option<Self> {
        let mut view = Self::default();
        let mut calls = HashSet::new();
        for item in items {
            if item
                .get("status")
                .is_some_and(|status| !status.is_null() && status != "completed")
            {
                return None;
            }
            if is_message(item, "assistant") {
                text_parts(
                    &item["content"],
                    &["output_text", "input_text", "text"],
                    &mut view.text,
                    &mut view.opaque,
                )?;
            } else {
                match item["type"].as_str()? {
                    "reasoning" => view.reasoning.push(item),
                    "function_call" | "custom_tool_call" => {
                        let call = ToolCall::read(item)?;
                        if !calls.insert(call.id) {
                            return None;
                        }
                        view.calls.push(call);
                    }
                    "message" => return None,
                    _ => view.opaque.push(item),
                }
            }
        }
        Some(view)
    }
}

/// 已知原生 ID 优先定位；重建 ID 用完整显式事实按顺序认领单项，不能跨项拼接摘要
fn reasoning_matches(native: &[&Value], rebuilt: &[&Value]) -> bool {
    let ids: HashSet<_> = native
        .iter()
        .filter_map(|item| item["id"].as_str())
        .collect();
    let mut remaining = native.iter();
    rebuilt.iter().all(|item| {
        let id = item["id"].as_str().filter(|id| ids.contains(id));
        remaining.any(|old| {
            id.is_none_or(|id| old["id"] == id)
                && item
                    .get("encrypted_content")
                    .filter(|v| !v.is_null())
                    .is_none_or(|encrypted| old.get("encrypted_content") == Some(encrypted))
                && reasoning_display_matches(old, item, "summary", "summary_text")
                && reasoning_display_matches(old, item, "content", "reasoning_text")
                && item_fields_match(
                    item,
                    &[*old],
                    &[
                        "type",
                        "id",
                        "status",
                        "summary",
                        "content",
                        "encrypted_content",
                    ],
                )
        })
    })
}

fn reasoning_display_matches(native: &Value, rebuilt: &Value, key: &str, kind: &str) -> bool {
    let Some(value) = rebuilt.get(key).filter(|value| !value.is_null()) else {
        return true;
    };
    let (mut text, mut opaque) = (Vec::new(), Vec::new());
    if text_parts(value, &[kind], &mut text, &mut opaque).is_none() {
        return false;
    }
    if text.is_empty() && opaque.is_empty() {
        return true;
    }
    let (mut old_text, mut old_opaque) = (Vec::new(), Vec::new());
    text_parts(&native[key], &[kind], &mut old_text, &mut old_opaque).is_some()
        && text_equal(&old_text, &text)
        && is_subsequence(&old_opaque, &opaque)
}

fn text_parts<'a>(
    value: &'a Value,
    kinds: &[&str],
    text: &mut Vec<&'a str>,
    opaque: &mut Vec<&'a Value>,
) -> Option<()> {
    if let Some(value) = value.as_str() {
        text.push(value);
    } else {
        for part in value.as_array()? {
            if kinds.contains(&part["type"].as_str()?) {
                text.push(part["text"].as_str()?);
            } else {
                opaque.push(part);
            }
        }
    }
    Some(())
}

fn text_equal(left: &[&str], right: &[&str]) -> bool {
    left.iter()
        .flat_map(|text| text.bytes())
        .eq(right.iter().flat_map(|text| text.bytes()))
}

fn is_subsequence(native: &[&Value], rebuilt: &[&Value]) -> bool {
    let mut native = native.iter();
    rebuilt.iter().all(|value| native.any(|old| old == value))
}

/// 缺省展示字段由原生项拥有；客户端显式提交的未知字段、签名和非空标注不能与原生冲突
fn explicit_fields_match(native: &[Value], rebuilt: &[Value]) -> bool {
    let mut native_kinds: HashMap<&str, Vec<&Value>> = HashMap::new();
    let mut client_counts = HashMap::new();
    for item in native {
        native_kinds
            .entry(item["type"].as_str().unwrap_or("message"))
            .or_default()
            .push(item);
    }
    for item in rebuilt {
        *client_counts
            .entry(item["type"].as_str().unwrap_or("message"))
            .or_insert(0_usize) += 1;
    }
    let mut positions = HashMap::new();
    rebuilt.iter().all(|item| {
        let known: &[&str] = if is_message(item, "assistant") {
            &["type", "role", "id", "status", "content"]
        } else {
            match item["type"].as_str() {
                // reasoning 的显式字段由已认领的单项核实，不按剩余项数推测位置
                Some("reasoning") => return true,
                Some("function_call") => &["type", "id", "status", "call_id", "name", "arguments"],
                Some("custom_tool_call") => &["type", "id", "status", "call_id", "name", "input"],
                _ => return native.contains(item),
            }
        };
        let kind = item["type"].as_str().unwrap_or("message");
        let candidates = native_kinds
            .get(kind)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let position = positions.entry(kind).or_insert(0_usize);
        let candidates = if candidates.len() == client_counts[&kind] {
            &candidates[*position..*position + 1]
        } else {
            candidates
        };
        *position += 1;
        item_fields_match(item, candidates, known)
    })
}

fn item_fields_match(item: &Value, candidates: &[&Value], known: &[&str]) -> bool {
    if !fields_present_match(item, candidates, known, &[]) {
        return false;
    }
    for key in ["content", "summary"] {
        if let Some(parts) = item.get(key).and_then(Value::as_array) {
            let native_parts: Vec<_> = candidates
                .iter()
                .filter_map(|old| old.get(key).and_then(Value::as_array))
                .flatten()
                .collect();
            for (index, part) in parts.iter().enumerate() {
                let candidates = if native_parts.len() == parts.len() {
                    &native_parts[index..index + 1]
                } else {
                    &native_parts
                };
                let kind = part["type"].as_str();
                if matches!(
                    kind,
                    Some("output_text" | "input_text" | "text" | "summary_text" | "reasoning_text")
                ) {
                    let optional: &[&str] =
                        if matches!(kind, Some("output_text" | "input_text" | "text")) {
                            &["annotations", "logprobs"]
                        } else {
                            &[]
                        };
                    if !fields_present_match(part, candidates, &["type", "text"], optional) {
                        return false;
                    }
                } else if !candidates.contains(&part) {
                    return false;
                }
            }
        }
    }
    true
}

fn fields_present_match(
    item: &Value,
    native: &[&Value],
    known: &[&str],
    optional: &[&str],
) -> bool {
    let Some(object) = item.as_object() else {
        return false;
    };
    let fields: Vec<_> = object
        .iter()
        .filter(|(key, value)| {
            !known.contains(&key.as_str())
                && !(optional.contains(&key.as_str())
                    && (value.is_null() || value.as_array().is_some_and(Vec::is_empty)))
        })
        .collect();
    fields.is_empty()
        || (!native.is_empty()
            && native.iter().all(|old| {
                fields
                    .iter()
                    .all(|(key, value)| old.get(*key) == Some(*value))
            }))
}

fn map_tail(
    tail: &mut [Value],
    aliases: HashMap<&str, ToolCall<'_>>,
    historical: HashSet<&str>,
    native_ids: HashSet<&str>,
) -> bool {
    let mut seen: HashSet<String> = historical
        .into_iter()
        .chain(native_ids)
        .chain(aliases.keys().copied())
        .map(str::to_owned)
        .collect();
    let mut outstanding: HashMap<_, _> = aliases
        .into_iter()
        .map(|(alias, native)| {
            (
                alias.to_owned(),
                (native.kind, native.id.to_owned(), native.name.to_owned()),
            )
        })
        .collect();
    let mut resolving_previous = !outstanding.is_empty();
    for item in tail {
        if let Some(kind) = ToolKind::result(item) {
            let Some(call) = item["call_id"].as_str() else {
                return false;
            };
            let Some((expected_kind, native, name)) = outstanding.remove(call) else {
                return false;
            };
            if kind != expected_kind
                || (kind == ToolKind::Custom
                    && item
                        .get("name")
                        .filter(|v| !v.is_null())
                        .is_some_and(|value| value.as_str() != Some(name.as_str())))
            {
                return false;
            }
            if native != call {
                item["call_id"] = Value::String(native);
            }
            resolving_previous &= !outstanding.is_empty();
        } else if is_tool_call(item) && !resolving_previous {
            let Some(call) = ToolCall::read(item) else {
                return false;
            };
            if !seen.insert(call.id.to_owned()) {
                return false;
            }
            outstanding.insert(
                call.id.to_owned(),
                (call.kind, call.id.to_owned(), call.name.to_owned()),
            );
        } else if !outstanding.is_empty() {
            return false;
        }
    }
    outstanding.is_empty()
}
