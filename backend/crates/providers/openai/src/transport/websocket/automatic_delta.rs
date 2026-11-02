//! 同一存活上游连接内的有界完整历史比较与原生完成项基线

use std::{
    borrow::Cow,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PrefixFailure {
    InputShorter,
    PriorInputChanged,
    PriorOutputMetadataOnly,
    PriorOutputOther,
}

impl PrefixFailure {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::InputShorter => "input_shorter",
            Self::PriorInputChanged => "prior_input_changed",
            Self::PriorOutputMetadataOnly => "prior_output_metadata_only",
            Self::PriorOutputOther => "prior_output_other",
        }
    }
}

pub(crate) struct AutomaticDeltaPreparation {
    pub(crate) delta: Option<(String, Vec<Value>)>,
    pub(crate) reason: &'static str,
    pub(crate) prefix_failure: Option<PrefixFailure>,
}

impl AutomaticDeltaPreparation {
    fn full(reason: &'static str) -> Self {
        Self {
            delta: None,
            reason,
            prefix_failure: None,
        }
    }

    fn prefix_failure(failure: PrefixFailure) -> Self {
        Self {
            delta: None,
            reason: "prefix_changed",
            prefix_failure: Some(failure),
        }
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
    ) -> AutomaticDeltaPreparation {
        self.pending = None;
        let previous = self.baseline.take();
        if !enabled
            || body
                .get("previous_response_id")
                .is_some_and(|id| !id.is_null())
            || body.get("generate") == Some(&Value::Bool(false))
        {
            return AutomaticDeltaPreparation::full("ineligible");
        }
        let Some(input) = body.get("input").and_then(Value::as_array) else {
            return AutomaticDeltaPreparation::full("input_not_array");
        };
        let mut prepared = match previous.as_ref() {
            Some(previous) if previous.scope.as_ref() != scope => {
                AutomaticDeltaPreparation::full("credential_revision_changed")
            }
            Some(previous) if Some(previous.response_id.as_ref()) != latest_response_id => {
                AutomaticDeltaPreparation::full("owner_changed")
            }
            Some(previous) => compare(previous, body, input, codex),
            None => AutomaticDeltaPreparation::full("no_baseline"),
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
        if self.pending.is_none() && prepared.delta.is_none() {
            prepared.reason = "budget_exhausted";
            prepared.prefix_failure = None;
        }
        prepared
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
) -> AutomaticDeltaPreparation {
    let Ok(previous_body) = serde_json::from_str::<Map<String, Value>>(&previous.request) else {
        return AutomaticDeltaPreparation::full("invalid_baseline");
    };
    if properties(&previous_body) != properties(body) {
        return AutomaticDeltaPreparation::full("properties_changed");
    }
    let Some(previous_input) = previous_body.get("input").and_then(Value::as_array) else {
        return AutomaticDeltaPreparation::full("invalid_baseline");
    };
    let count = previous_input.len().saturating_add(previous.output.len());
    if input.len() < count {
        return AutomaticDeltaPreparation::prefix_failure(PrefixFailure::InputShorter);
    }
    if !previous_input
        .iter()
        .zip(input)
        .all(|(old, new)| items_match(old, new, codex))
    {
        return AutomaticDeltaPreparation::prefix_failure(PrefixFailure::PriorInputChanged);
    }
    let mut metadata_only = false;
    for (old, new) in previous
        .output
        .iter()
        .zip(&input[previous_input.len()..count])
    {
        let Ok(old) = serde_json::from_str::<Value>(old) else {
            return AutomaticDeltaPreparation::full("invalid_baseline");
        };
        if old == *new {
            continue;
        }
        let old = project_item(&old, codex);
        let new = project_item(new, codex);
        if old == new {
            continue;
        }
        if !output_metadata_matches(&old, &new) {
            return AutomaticDeltaPreparation::prefix_failure(PrefixFailure::PriorOutputOther);
        }
        // 继续核对其余完成项，不能让首项的元数据差异掩盖后续业务字段变化
        metadata_only = true;
    }
    if metadata_only {
        return AutomaticDeltaPreparation::prefix_failure(PrefixFailure::PriorOutputMetadataOnly);
    }
    AutomaticDeltaPreparation {
        delta: Some((previous.response_id.to_string(), input[count..].to_vec())),
        reason: "delta",
        prefix_failure: None,
    }
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
    previous == current || project_item(previous, codex) == project_item(current, codex)
}

fn project_item(item: &Value, codex: bool) -> Cow<'_, Value> {
    if item
        .get("internal_chat_message_metadata_passthrough")
        .is_none()
        && (!codex || item.get("type").and_then(Value::as_str) != Some("reasoning"))
    {
        return Cow::Borrowed(item);
    }
    let mut item = item.clone();
    if let Some(object) = item.as_object_mut() {
        object.shift_remove("internal_chat_message_metadata_passthrough");
    }
    if codex {
        crate::transport::downstream::normalize_reasoning_input_item(&mut item);
    }
    Cow::Owned(item)
}

fn output_metadata_matches(previous: &Value, current: &Value) -> bool {
    let (Some(previous), Some(current)) = (previous.as_object(), current.as_object()) else {
        return false;
    };
    // 只分型已知位置，不递归忽略未知业务对象中碰巧同名的字段
    if !objects_match_without(previous, current, &["id", "status", "content"]) {
        return false;
    }
    match (previous.get("content"), current.get("content")) {
        (Some(Value::Array(old)), Some(Value::Array(new)))
            if previous.get("type").and_then(Value::as_str) == Some("message") =>
        {
            old.len() == new.len()
                && old
                    .iter()
                    .zip(new)
                    .all(|(old, new)| match (old.as_object(), new.as_object()) {
                        (Some(old), Some(new))
                            if old.get("type").and_then(Value::as_str) == Some("output_text") =>
                        {
                            objects_match_without(old, new, &["annotations", "logprobs"])
                        }
                        _ => old == new,
                    })
        }
        (old, new) => old == new,
    }
}

fn objects_match_without(
    previous: &Map<String, Value>,
    current: &Map<String, Value>,
    ignored: &[&str],
) -> bool {
    previous
        .iter()
        .all(|(key, value)| ignored.contains(&key.as_str()) || current.get(key) == Some(value))
        && current
            .keys()
            .all(|key| ignored.contains(&key.as_str()) || previous.contains_key(key))
}
