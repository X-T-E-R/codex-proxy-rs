//! OpenAI 请求环境、搜索位置与设备 metadata 覆盖策略。

use std::sync::{Arc, RwLock};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use gateway_core::provider_ports::OpenAiRequestBodyOverride;
use gateway_core::routing::{
    ModelRequestPolicy, ReasoningEffort, RequestedReasoningEffort, ServiceTierRule,
};
use roxmltree::{Document, Node};
use serde_json::{Map, Value};

use super::protocol::responses::CodexResponsesRequest;

const ENVIRONMENT_CONTEXT: &str = "environment_context";
const CURRENT_DATE: &str = "current_date";
const TIMEZONE: &str = "timezone";

/// Provider worker 热更新、请求执行路径无锁外持有的正文覆盖快照。
#[derive(Debug, Clone)]
pub struct CodexRequestBodyOverrideState {
    policy: Arc<RwLock<OpenAiRequestBodyOverride>>,
}

impl CodexRequestBodyOverrideState {
    #[must_use]
    pub fn new(policy: OpenAiRequestBodyOverride) -> Self {
        Self {
            policy: Arc::new(RwLock::new(policy)),
        }
    }

    /// 返回独立快照；同一次 attempt 的 HTTP、WebSocket 与内部重试共用它。
    #[must_use]
    pub fn snapshot(&self) -> OpenAiRequestBodyOverride {
        self.policy
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn update(&self, policy: OpenAiRequestBodyOverride) {
        *self
            .policy
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = policy;
    }
}

/// 应用路由计划冻结的模型请求策略：改写推理强度与服务档位。
///
/// 需要写入时创建缺失的 `reasoning` 对象；其他字段与顺序保持不变。
/// 返回值表示正文是否发生变化。
pub fn apply_model_request_policy(
    request: &mut CodexResponsesRequest,
    policy: &ModelRequestPolicy,
) -> bool {
    let body = request.body_mut();
    let mut changed = false;
    if let Some(rule) = policy.reasoning_effort() {
        let requested = match body
            .get("reasoning")
            .and_then(Value::as_object)
            .and_then(|reasoning| reasoning.get("effort"))
        {
            None => RequestedReasoningEffort::Absent,
            Some(value) => value
                .as_str()
                .map_or(RequestedReasoningEffort::Unknown, |effort| {
                    ReasoningEffort::parse(effort).map_or(
                        RequestedReasoningEffort::Unknown,
                        RequestedReasoningEffort::Known,
                    )
                }),
        };
        if let Some(effort) = rule.resolve(requested) {
            // `reasoning: null` 视为缺失对象直接替换；其他非对象形状保持原样不写入。
            if body.get("reasoning").is_some_and(Value::is_null) {
                body.insert("reasoning".to_owned(), Value::Object(Map::new()));
            }
            let reasoning = body
                .entry("reasoning".to_owned())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Some(object) = reasoning.as_object_mut()
                && object.get("effort").and_then(Value::as_str) != Some(effort.as_str())
            {
                object.insert(
                    "effort".to_owned(),
                    Value::String(effort.as_str().to_owned()),
                );
                changed = true;
            }
        }
    }
    match policy.service_tier() {
        Some(ServiceTierRule::LockFast) => {
            if body.get("service_tier").and_then(Value::as_str) != Some("fast") {
                body.insert("service_tier".to_owned(), Value::String("fast".to_owned()));
                changed = true;
            }
        }
        Some(ServiceTierRule::LockNeverFast) => {
            let requests_fast = body
                .get("service_tier")
                .and_then(Value::as_str)
                .is_some_and(|tier| {
                    matches!(
                        tier.trim().to_ascii_lowercase().as_str(),
                        "fast" | "priority"
                    )
                });
            if requests_fast {
                body.remove("service_tier");
                changed = true;
            }
        }
        None => {}
    }
    changed
}

/// 改写已有 Responses 请求中的最新专用环境块与已声明搜索工具。
///
/// 返回值表示正文是否发生变化；关闭开关或未命中时不写入 Map。
pub fn apply_responses_request_override(
    request: &mut CodexResponsesRequest,
    policy: &OpenAiRequestBodyOverride,
    captured_at: DateTime<Utc>,
) -> bool {
    if !policy.enabled() {
        return false;
    }
    let Ok(timezone) = policy.timezone().parse::<Tz>() else {
        return false;
    };
    let current_date = captured_at
        .with_timezone(&timezone)
        .format("%Y-%m-%d")
        .to_string();
    let timezone = policy.timezone();
    let country = policy.search_country();

    let body = request.body_mut();
    let environment_changed = body
        .get_mut("input")
        .is_some_and(|input| rewrite_latest_environment_context(input, &current_date, timezone));
    let tools_changed = body
        .get_mut("tools")
        .is_some_and(|tools| rewrite_web_search_tools(tools, country, timezone));
    environment_changed || tools_changed
}

/// 改写已确定为 standalone Search operation 的正文。
///
/// missing/null settings 与 user_location 按官方 schema 创建；非 object 值保持原样。
/// 关闭或无需变化时返回原 Bytes clone，保持字节内容一致。
#[must_use]
pub fn apply_standalone_search_override(body: &Bytes, policy: &OpenAiRequestBodyOverride) -> Bytes {
    if !policy.enabled() {
        return body.clone();
    }
    let Ok(Value::Object(mut root)) = serde_json::from_slice::<Value>(body) else {
        return body.clone();
    };
    let Some(settings) = object_field_or_insert(&mut root, "settings") else {
        return body.clone();
    };
    let Some(location) = object_field_or_insert(settings, "user_location") else {
        return body.clone();
    };
    if !rewrite_location(location, policy.search_country(), policy.timezone()) {
        return body.clone();
    }
    serde_json::to_vec(&Value::Object(root))
        .map(Bytes::from)
        .unwrap_or_else(|_| body.clone())
}

fn rewrite_latest_environment_context(
    input: &mut Value,
    current_date: &str,
    timezone: &str,
) -> bool {
    let Some(items) = input.as_array_mut() else {
        return false;
    };
    for item in items.iter_mut().rev() {
        let Some(text) = dedicated_environment_context_text(item) else {
            continue;
        };
        match rewrite_environment_context_xml(text, current_date, timezone) {
            EnvironmentContextRewrite::NotEnvironmentContext => continue,
            EnvironmentContextRewrite::Unchanged => return false,
            EnvironmentContextRewrite::Rewritten(rewritten) => {
                *text = rewritten;
                return true;
            }
        }
    }
    false
}

fn dedicated_environment_context_text(item: &mut Value) -> Option<&mut String> {
    let item = item.as_object_mut()?;
    if item.get("type").and_then(Value::as_str) != Some("message")
        || item.get("role").and_then(Value::as_str) != Some("user")
    {
        return None;
    }
    let content = item.get_mut("content")?.as_array_mut()?;
    let [part] = content.as_mut_slice() else {
        return None;
    };
    let part = part.as_object_mut()?;
    if part.get("type").and_then(Value::as_str) != Some("input_text") {
        return None;
    }
    part.get_mut("text")?.as_str().map(|_| ())?;
    match part.get_mut("text")? {
        Value::String(text) => Some(text),
        _ => None,
    }
}

enum EnvironmentContextRewrite {
    NotEnvironmentContext,
    Unchanged,
    Rewritten(String),
}

fn rewrite_environment_context_xml(
    text: &str,
    current_date: &str,
    timezone: &str,
) -> EnvironmentContextRewrite {
    let Ok(document) = Document::parse(text) else {
        return EnvironmentContextRewrite::NotEnvironmentContext;
    };
    let root = document.root_element();
    if root.tag_name().namespace().is_some() || root.tag_name().name() != ENVIRONMENT_CONTEXT {
        return EnvironmentContextRewrite::NotEnvironmentContext;
    }
    if root.attributes().len() != 0 {
        return EnvironmentContextRewrite::Unchanged;
    }
    let Some(current_date_node) = unique_direct_text_element(root, CURRENT_DATE) else {
        return EnvironmentContextRewrite::Unchanged;
    };
    let Some(timezone_node) = unique_direct_text_element(root, TIMEZONE) else {
        return EnvironmentContextRewrite::Unchanged;
    };
    if current_date_node.text() == Some(current_date) && timezone_node.text() == Some(timezone) {
        return EnvironmentContextRewrite::Unchanged;
    }

    let mut replacements = [
        (
            current_date_node.range(),
            format!("<{CURRENT_DATE}>{current_date}</{CURRENT_DATE}>"),
        ),
        (
            timezone_node.range(),
            format!("<{TIMEZONE}>{timezone}</{TIMEZONE}>"),
        ),
    ];
    replacements.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
    let mut rewritten = text.to_owned();
    for (range, replacement) in replacements {
        rewritten.replace_range(range, &replacement);
    }
    EnvironmentContextRewrite::Rewritten(rewritten)
}

fn unique_direct_text_element<'a>(root: Node<'a, 'a>, name: &str) -> Option<Node<'a, 'a>> {
    let mut matches = root
        .children()
        .filter(Node::is_element)
        .filter(|node| node.tag_name().namespace().is_none() && node.tag_name().name() == name);
    let node = matches.next()?;
    let mut children = node.children();
    if matches.next().is_some()
        || node.attributes().len() != 0
        || children.next().is_none_or(|child| !child.is_text())
        || children.next().is_some()
    {
        return None;
    }
    Some(node)
}

fn rewrite_web_search_tools(tools: &mut Value, country: &str, timezone: &str) -> bool {
    let Some(tools) = tools.as_array_mut() else {
        return false;
    };
    let mut changed = false;
    for tool in tools {
        let Some(tool) = tool.as_object_mut() else {
            continue;
        };
        let Some(tool_type) = tool.get("type").and_then(Value::as_str) else {
            continue;
        };
        if !is_web_search_tool_type(tool_type) {
            continue;
        }
        let Some(location) = object_field_or_insert(tool, "user_location") else {
            continue;
        };
        changed |= rewrite_location(location, country, timezone);
    }
    changed
}

fn is_web_search_tool_type(value: &str) -> bool {
    value == "web_search"
        || value == "web_search_preview"
        || value.starts_with("web_search_preview_")
}

fn object_field_or_insert<'a>(
    parent: &'a mut Map<String, Value>,
    key: &str,
) -> Option<&'a mut Map<String, Value>> {
    if parent.get(key).is_none_or(Value::is_null) {
        parent.insert(key.to_owned(), Value::Object(Map::new()));
    }
    parent.get_mut(key)?.as_object_mut()
}

fn rewrite_location(location: &mut Map<String, Value>, country: &str, timezone: &str) -> bool {
    let desired = [
        ("type", "approximate"),
        ("country", country),
        ("timezone", timezone),
    ];
    let mut changed = false;
    for (key, value) in desired {
        if location.get(key).and_then(Value::as_str) != Some(value) {
            location.insert(key.to_owned(), Value::String(value.to_owned()));
            changed = true;
        }
    }
    changed |= location.shift_remove("region").is_some();
    changed |= location.shift_remove("city").is_some();
    changed
}
