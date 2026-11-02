//! 同一存活上游连接内的有界完整历史比较与原生完成项基线

use std::{
    collections::BTreeMap,
    fmt,
    sync::atomic::{AtomicUsize, Ordering},
};

use serde_json::{Map, Value};

// 常驻快照保留编码后的原生 JSON，避免解析树的节点开销放大内存预算
const MAX_BASELINE_BYTES: usize = 32 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 256 * 1024 * 1024;
const MAX_OUTPUT_ITEMS: usize = 4096;
static RETAINED_BYTES: AtomicUsize = AtomicUsize::new(0);

struct Budget(usize);

impl Budget {
    fn reserve(&mut self, bytes: usize) -> bool {
        if self.0.saturating_add(bytes) > MAX_BASELINE_BYTES {
            return false;
        }
        if RETAINED_BYTES
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(bytes)
                    .filter(|next| *next <= MAX_TOTAL_BYTES)
            })
            .is_err()
        {
            return false;
        }
        self.0 += bytes;
        true
    }

    fn release(&mut self, bytes: usize) {
        self.0 -= bytes;
        RETAINED_BYTES.fetch_sub(bytes, Ordering::Relaxed);
    }
}

impl Drop for Budget {
    fn drop(&mut self) {
        RETAINED_BYTES.fetch_sub(self.0, Ordering::Relaxed);
    }
}

struct Baseline {
    request: Box<str>,
    scope: Box<str>,
    output: Vec<Box<str>>,
    response_id: Box<str>,
    _budget: Budget,
}

struct Pending {
    request: Box<str>,
    scope: Box<str>,
    items: BTreeMap<u64, Box<str>>,
    response_id: Option<Box<str>>,
    output_seen: bool,
    max_output_index: Option<u64>,
    budget: Budget,
}

#[derive(Default)]
pub(crate) struct AutomaticDeltaState {
    baseline: Option<Baseline>,
    pending: Option<Pending>,
}

impl fmt::Debug for AutomaticDeltaState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AutomaticDeltaState")
            .field("has_baseline", &self.baseline.is_some())
            .field("has_pending", &self.pending.is_some())
            .finish()
    }
}

impl AutomaticDeltaState {
    /// 消费上一轮基线并为本轮保留逻辑 full，不把已发送的 suffix 当成下轮历史
    pub(crate) fn prepare(
        &mut self,
        body: &Map<String, Value>,
        full_payload: &str,
        latest_response_id: Option<&str>,
        codex: bool,
        enabled: bool,
        scope: &str,
    ) -> (Option<(String, Vec<Value>)>, &'static str) {
        self.pending = None;
        let previous = self.baseline.take();
        if !enabled
            || body
                .get("previous_response_id")
                .is_some_and(|id| !id.is_null())
            || body.get("generate") == Some(&Value::Bool(false))
        {
            return (None, "ineligible");
        }
        let Some(input) = body.get("input").and_then(Value::as_array) else {
            return (None, "input_not_array");
        };
        let (delta, reason) = match previous.as_ref() {
            Some(previous) if previous.scope.as_ref() != scope => {
                (None, "credential_revision_changed")
            }
            Some(previous) if Some(previous.response_id.as_ref()) != latest_response_id => {
                (None, "owner_changed")
            }
            Some(previous) => compare(previous, body, input, codex),
            None => (None, "no_baseline"),
        };
        drop(previous);
        let mut budget = Budget(0);
        if budget.reserve(full_payload.len().saturating_add(scope.len())) {
            self.pending = Some(Pending {
                request: full_payload.into(),
                scope: scope.into(),
                items: BTreeMap::new(),
                response_id: None,
                output_seen: false,
                max_output_index: None,
                budget,
            });
        }
        let reason = if self.pending.is_none() && delta.is_none() {
            "budget_exhausted"
        } else {
            reason
        };
        (delta, reason)
    }

    pub(crate) fn has_baseline(&self) -> bool {
        self.baseline.is_some()
    }

    /// 旁路读取原生事件，只有完整 completed 才能发布下一轮基线
    pub(crate) fn observe(&mut self, value: &Value) {
        let Some(pending) = self.pending.as_mut() else {
            return;
        };
        let event = value.get("type").and_then(Value::as_str);
        if let Some(index) = value.get("output_index").and_then(Value::as_u64) {
            pending.max_output_index = Some(
                pending
                    .max_output_index
                    .map_or(index, |previous| previous.max(index)),
            );
        }
        if let Some(id) = value
            .pointer("/response/id")
            .or_else(|| value.get("response_id"))
            .and_then(Value::as_str)
        {
            if pending
                .response_id
                .as_deref()
                .is_some_and(|known| known != id)
            {
                self.pending = None;
                return;
            }
            if pending.response_id.is_none() {
                if !pending.budget.reserve(id.len()) {
                    self.pending = None;
                    return;
                }
                pending.response_id = Some(id.into());
            }
        }
        match event {
            Some("response.output_item.done") => {
                let item = value.get("item").filter(|item| item.is_object());
                let index = value.get("output_index").and_then(Value::as_u64);
                match (index, item) {
                    (Some(index), Some(item)) => {
                        let item = item.to_string();
                        if let Some(previous) = pending.items.get(&index) {
                            if previous.as_ref() != item {
                                self.pending = None;
                            }
                        } else if pending.items.len() < MAX_OUTPUT_ITEMS
                            && pending.budget.reserve(item.len())
                        {
                            pending.items.insert(index, item.into_boxed_str());
                        } else {
                            self.pending = None;
                        }
                    }
                    _ => self.pending = None,
                }
            }
            Some("response.completed") => {
                let mut pending = self.pending.take().expect("pending completion");
                let Some(id) = value
                    .pointer("/response/id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                else {
                    return;
                };
                let output = match value.pointer("/response/output") {
                    Some(Value::Array(output)) if !output.is_empty() => {
                        if output.len() > MAX_OUTPUT_ITEMS || !output.iter().all(Value::is_object) {
                            return;
                        }
                        // 完整终态快照替代 done，不同时保留两份相同输出
                        let released = pending.items.values().map(|item| item.len()).sum();
                        pending.items.clear();
                        pending.budget.release(released);
                        let mut retained = Vec::with_capacity(output.len());
                        for item in output {
                            let item = item.to_string();
                            if !pending.budget.reserve(item.len()) {
                                return;
                            }
                            retained.push(item.into_boxed_str());
                        }
                        retained
                    }
                    None | Some(Value::Array(_)) => {
                        if (pending.output_seen && pending.items.is_empty())
                            || pending.max_output_index.is_some_and(|index| {
                                index.saturating_add(1) != pending.items.len() as u64
                            })
                            || pending
                                .items
                                .keys()
                                .enumerate()
                                .any(|(expected, actual)| expected as u64 != *actual)
                        {
                            return;
                        }
                        pending.items.into_values().collect()
                    }
                    _ => return,
                };
                let Some(response_id) = pending
                    .response_id
                    .filter(|response_id| response_id.as_ref() == id)
                else {
                    return;
                };
                self.baseline = Some(Baseline {
                    request: pending.request,
                    scope: pending.scope,
                    output,
                    response_id,
                    _budget: pending.budget,
                });
            }
            Some("response.incomplete" | "response.failed" | "error") => self.pending = None,
            Some(event)
                if event.starts_with("response.output_item.") || event.ends_with(".delta") =>
            {
                pending.output_seen = true
            }
            _ => {}
        }
    }

    pub(crate) fn invalidate(&mut self) {
        self.baseline = None;
        self.pending = None;
    }
}

fn compare(
    previous: &Baseline,
    body: &Map<String, Value>,
    input: &[Value],
    codex: bool,
) -> (Option<(String, Vec<Value>)>, &'static str) {
    let Ok(previous_body) = serde_json::from_str::<Map<String, Value>>(&previous.request) else {
        return (None, "invalid_baseline");
    };
    if properties(&previous_body) != properties(body) {
        return (None, "properties_changed");
    }
    let Some(previous_input) = previous_body.get("input").and_then(Value::as_array) else {
        return (None, "invalid_baseline");
    };
    let count = previous_input.len().saturating_add(previous.output.len());
    if input.len() < count
        || !previous_input
            .iter()
            .zip(input)
            .all(|(old, new)| items_match(old, new, codex))
    {
        return (None, "prefix_changed");
    }
    for (old, new) in previous
        .output
        .iter()
        .zip(&input[previous_input.len()..count])
    {
        let Ok(old) = serde_json::from_str::<Value>(old) else {
            return (None, "invalid_baseline");
        };
        if !items_match(&old, new, codex) {
            return (None, "prefix_changed");
        }
    }
    (
        Some((previous.response_id.to_string(), input[count..].to_vec())),
        "delta",
    )
}

fn properties(body: &Map<String, Value>) -> Map<String, Value> {
    let mut properties: Map<_, _> = body
        .iter()
        .filter(|(key, _)| {
            !matches!(
                key.as_str(),
                "input" | "client_metadata" | "stream_options" | "previous_response_id"
            )
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    properties
        .entry("type")
        .or_insert_with(|| Value::String("response.create".to_owned()));
    properties
}

fn items_match(previous: &Value, current: &Value, codex: bool) -> bool {
    if previous == current {
        return true;
    }
    let project = |item: &Value| {
        let mut item = item.clone();
        if let Some(object) = item.as_object_mut() {
            object.shift_remove("internal_chat_message_metadata_passthrough");
        }
        if codex {
            crate::transport::downstream::normalize_reasoning_input_item(&mut item);
        }
        item
    };
    project(previous) == project(current)
}
