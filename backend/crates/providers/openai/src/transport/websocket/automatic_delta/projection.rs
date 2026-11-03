//! 按新增轮边界认领客户端回填，原生输出拥有历史，显式业务冲突要求重建

use std::collections::{HashMap, HashSet};

use serde_json::Value;

pub(super) struct Claim {
    pub(super) tail: Vec<Value>,
}

/// 身份、旧 input、live lease 与最新 completed owner 已由调用方核实
/// user / tool-result 是新增轮边界，不按客户端字段模板推测 item 数量
pub(super) fn boundary(
    output: &[Value],
    current: &[Value],
    previous_input: &[Value],
    native_calls: &[String],
) -> Result<Claim, &'static str> {
    let count = current
        .iter()
        .position(|item| is_message(item, "user") || item["type"] == "function_call_output")
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
        || (!client.summary.is_empty() && !text_equal(&native.summary, &client.summary))
        || (!client.reasoning.is_empty() && !text_equal(&native.reasoning, &client.reasoning))
        || !is_subsequence(&native.encrypted, &client.encrypted)
        || !is_subsequence(&native.opaque, &client.opaque)
        || native.calls.len() != client.calls.len()
        || !explicit_fields_match(output, rebuilt)
    {
        return Err("output_conflict");
    }
    let mut aliases = HashMap::new();
    let native_ids: HashSet<_> = native.calls.iter().map(|call| call.0).collect();
    let historical: HashSet<_> = previous_input
        .iter()
        .filter(|item| item["type"] == "function_call")
        .filter_map(|item| item["call_id"].as_str())
        .chain(native_calls.iter().map(String::as_str))
        .collect();
    let mut signatures = HashMap::new();
    for call in &native.calls {
        *signatures.entry((call.1, call.2)).or_insert(0_usize) += 1;
    }
    for (old, new) in native.calls.iter().zip(&client.calls) {
        if old.1 != new.1
            || old.2 != new.2
            || (old.0 != new.0
                && (native_ids.contains(new.0)
                    || historical.contains(new.0)
                    || signatures[&(old.1, old.2)] != 1))
            || aliases.insert(new.0, old.0).is_some()
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

#[derive(Default)]
struct View<'a> {
    text: Vec<&'a str>,
    summary: Vec<&'a str>,
    reasoning: Vec<&'a str>,
    encrypted: Vec<&'a Value>,
    opaque: Vec<&'a Value>,
    calls: Vec<(&'a str, &'a str, &'a str)>,
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
                    "reasoning" => {
                        if let Some(summary) = item.get("summary") {
                            text_parts(
                                summary,
                                &["summary_text"],
                                &mut view.summary,
                                &mut view.opaque,
                            )?;
                        }
                        if let Some(content) = item.get("content") {
                            text_parts(
                                content,
                                &["reasoning_text"],
                                &mut view.reasoning,
                                &mut view.opaque,
                            )?;
                        }
                        if let Some(encrypted) =
                            item.get("encrypted_content").filter(|v| !v.is_null())
                        {
                            view.encrypted.push(encrypted);
                        }
                    }
                    "function_call" => {
                        let call = function_call(item)?;
                        if !calls.insert(call.0) {
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

fn function_call(item: &Value) -> Option<(&str, &str, &str)> {
    Some((
        item["call_id"].as_str().filter(|id| !id.is_empty())?,
        item["name"].as_str().filter(|name| !name.is_empty())?,
        item["arguments"].as_str()?,
    ))
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
                Some("reasoning") => &[
                    "type",
                    "id",
                    "status",
                    "summary",
                    "content",
                    "encrypted_content",
                ],
                Some("function_call") => &["type", "id", "status", "call_id", "name", "arguments"],
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
                        Some(
                            "output_text"
                                | "input_text"
                                | "text"
                                | "summary_text"
                                | "reasoning_text"
                        )
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
    })
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
    aliases: HashMap<&str, &str>,
    historical: HashSet<&str>,
    native_ids: HashSet<&str>,
) -> bool {
    let mut seen: HashSet<String> = historical
        .into_iter()
        .chain(native_ids)
        .chain(aliases.keys().copied())
        .map(str::to_owned)
        .collect();
    let mut outstanding: HashMap<String, String> = aliases
        .into_iter()
        .map(|(alias, native)| (alias.to_owned(), native.to_owned()))
        .collect();
    let mut resolving_previous = !outstanding.is_empty();
    for item in tail {
        match item["type"].as_str() {
            Some("function_call_output") => {
                let Some(call) = item["call_id"].as_str() else {
                    return false;
                };
                let Some(native) = outstanding.remove(call) else {
                    return false;
                };
                if native != call {
                    item["call_id"] = Value::String(native);
                }
                resolving_previous &= !outstanding.is_empty();
            }
            Some("function_call") if !resolving_previous => {
                let Some((call, _, _)) = function_call(item) else {
                    return false;
                };
                if !seen.insert(call.to_owned()) {
                    return false;
                }
                outstanding.insert(call.to_owned(), call.to_owned());
            }
            _ if outstanding.is_empty() => {}
            _ => return false,
        }
    }
    outstanding.is_empty()
}
