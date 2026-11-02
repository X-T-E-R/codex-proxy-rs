//! 从原生完成项生成确定的客户端回填模板，校验工具关联与新增尾项

use std::collections::HashSet;

use serde_json::{Value, json};

#[derive(Clone, Copy)]
enum Style {
    Responses,
    Typed,
}

enum Part {
    Text(String),
    Think(String, Option<Value>),
}

/// 模板只用于认领最近原生输出，不改变发给上游的原生内容或顺序
pub(super) fn boundary(
    output: &[Value],
    current: &[Value],
    previous_input: &[Value],
) -> Result<(usize, &'static str), &'static str> {
    let mut matched = None;
    for (style, name) in [(Style::Responses, "responses"), (Style::Typed, "typed")] {
        let Some(template) = project(output, style) else {
            continue;
        };
        if template.is_empty() || !current.starts_with(&template) {
            continue;
        }
        if matched.is_some_and(|(count, _)| count != template.len()) {
            return Err("ambiguous_projection");
        }
        matched = Some((template.len(), name));
    }
    let (count, name) = matched.ok_or("projection_mismatch")?;
    if !tail_valid(output, &current[count..], previous_input) {
        return Err("tool_tail_invalid");
    }
    Ok((count, name))
}

fn function_call(item: &Value) -> Option<(&str, &str, &str)> {
    let object = item.as_object()?;
    if item["type"] != "function_call"
        || object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "type" | "id" | "status" | "call_id" | "name" | "arguments"
            )
        })
    {
        return None;
    }
    let call = item["call_id"].as_str().filter(|s| !s.is_empty())?;
    let name = item["name"].as_str().filter(|s| !s.is_empty())?;
    let arguments = item["arguments"].as_str().filter(|s| !s.is_empty())?;
    serde_json::from_str::<Value>(arguments).ok()?.as_object()?;
    Some((call, name, arguments))
}

fn projectable(output: &[Value]) -> bool {
    let mut anchor = false;
    let mut reasoning = false;
    let mut calls = HashSet::new();
    for item in output {
        if item
            .get("status")
            .is_some_and(|status| !status.is_null() && status != "completed")
        {
            return false;
        }
        match item["type"].as_str() {
            Some("message") => {
                if item["role"] != "assistant" {
                    return false;
                }
                let Some(content) = item["content"].as_array() else {
                    return false;
                };
                for part in content {
                    let Some(text) = part["text"].as_str() else {
                        return false;
                    };
                    if part["type"] != "output_text"
                        || ["annotations", "logprobs"].iter().any(|key| {
                            part.get(key).is_some_and(|value| {
                                !value.is_null()
                                    && value.as_array().is_none_or(|array| !array.is_empty())
                            })
                        })
                    {
                        return false;
                    }
                    anchor |= !text.is_empty();
                }
            }
            Some("reasoning") => {
                if reasoning {
                    return false;
                }
                reasoning = true;
                if item["summary"].as_array().is_none_or(|summary| {
                    summary
                        .iter()
                        .any(|part| part["type"] != "summary_text" || !part["text"].is_string())
                }) {
                    return false;
                }
            }
            Some("function_call") => {
                let Some((call, _, _)) = function_call(item) else {
                    return false;
                };
                if !calls.insert(call) {
                    return false;
                }
                anchor = true;
            }
            _ => return false,
        }
    }
    anchor
}

fn project(output: &[Value], style: Style) -> Option<Vec<Value>> {
    if !projectable(output) {
        return None;
    }
    let mut parts = Vec::new();
    let mut calls = Vec::new();
    let mut barrier = false;
    for item in output {
        match item["type"].as_str()? {
            "function_call" => {
                let (call, name, arguments) = function_call(item)?;
                calls.push(json!({"type":"function_call","call_id":call,"name":name,"arguments":arguments}));
                barrier = true;
            }
            "message" => {
                for part in item["content"].as_array()? {
                    let text = part["text"].as_str()?;
                    if text.is_empty() {
                        continue;
                    }
                    if let Some(Part::Text(previous)) = parts.last_mut()
                        && (matches!(style, Style::Typed) || !barrier)
                    {
                        previous.push_str(text);
                    } else {
                        parts.push(Part::Text(text.to_owned()));
                    }
                    barrier = false;
                }
            }
            "reasoning" => {
                let encrypted = item.get("encrypted_content").cloned();
                if matches!(style, Style::Typed)
                    && encrypted
                        .as_ref()
                        .and_then(Value::as_str)
                        .is_none_or(|s| !s.trim_matches('"').starts_with("gAAA"))
                {
                    continue;
                }
                let summary = item["summary"]
                    .as_array()?
                    .iter()
                    .map(|part| part["text"].as_str())
                    .collect::<Option<String>>()?;
                parts.push(Part::Think(summary, encrypted));
                barrier = false;
            }
            _ => return None,
        }
    }
    if matches!(style, Style::Typed) {
        let text: String = parts
            .iter()
            .filter_map(|part| match part {
                Part::Text(text) => Some(text.as_str()),
                Part::Think(..) => None,
            })
            .collect();
        let has_text = parts.iter().any(|part| matches!(part, Part::Text(_)));
        parts.retain(|part| matches!(part, Part::Think(..)));
        if has_text {
            parts.push(Part::Text(text));
        }
    }
    let mut result = Vec::new();
    let mut pending = Vec::new();
    for part in parts {
        match part {
            Part::Think(text, encrypted) => {
                flush(&mut result, &mut pending, style);
                let mut item = json!({"type":"reasoning","summary":if text.is_empty() { vec![] } else { vec![json!({"type":"summary_text","text":text})] }});
                if let Some(encrypted) = encrypted {
                    item["encrypted_content"] = encrypted;
                }
                result.push(item);
            }
            Part::Text(text) => {
                let mut item = json!({"type":"output_text","text":text});
                if matches!(style, Style::Responses) {
                    item["annotations"] = json!([]);
                }
                pending.push(item);
            }
        }
    }
    flush(&mut result, &mut pending, style);
    result.extend(calls);
    Some(result)
}

fn flush(result: &mut Vec<Value>, pending: &mut Vec<Value>, style: Style) {
    if !pending.is_empty() {
        let mut message =
            json!({"type":"message","role":"assistant","content":std::mem::take(pending)});
        if matches!(style, Style::Typed) {
            message["status"] = json!("completed");
        }
        result.push(message);
    }
}

fn tail_valid<'a>(output: &'a [Value], tail: &'a [Value], previous_input: &'a [Value]) -> bool {
    if tail.is_empty() {
        return false;
    }
    let mut outstanding: HashSet<_> = output
        .iter()
        .filter(|item| item["type"] == "function_call")
        .filter_map(|item| item["call_id"].as_str())
        .collect();
    if outstanding.is_empty() && (tail[0]["type"] != "message" || tail[0]["role"] != "user") {
        return false;
    }
    let mut seen = outstanding.clone();
    seen.extend(
        previous_input
            .iter()
            .filter(|item| item["type"] == "function_call")
            .filter_map(|item| item["call_id"].as_str()),
    );
    for item in tail {
        match item["type"].as_str() {
            Some("function_call_output") => {
                let Some(call) = item["call_id"].as_str() else {
                    return false;
                };
                if !outstanding.remove(call) {
                    return false;
                }
            }
            Some("function_call") => {
                let Some((call, _, _)) = function_call(item) else {
                    return false;
                };
                if !outstanding.is_empty() || !seen.insert(call) {
                    return false;
                }
                outstanding.insert(call);
            }
            Some("message")
                if outstanding.is_empty()
                    && matches!(item["role"].as_str(), Some("user" | "assistant")) => {}
            Some("reasoning") if outstanding.is_empty() => {}
            _ => return false,
        }
    }
    outstanding.is_empty()
}
