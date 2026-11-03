//! 同一存活上游连接内的有界完整历史比较与原生完成项基线

use std::{
    borrow::Cow,
    collections::BTreeMap,
    fmt,
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use serde_json::{Map, Value};

mod projection;

// 编码后的 client full 与原生替换段共享；恢复令牌也计入同一连接预算
const MAX_BASELINE_BYTES: usize = 32 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 256 * 1024 * 1024;
const MAX_OUTPUT_ITEMS: usize = 4096;
static RETAINED_BYTES: AtomicUsize = AtomicUsize::new(0);

struct Budget {
    bytes: usize,
    connection: Arc<AtomicUsize>,
}

impl Budget {
    fn new(connection: Arc<AtomicUsize>) -> Self {
        Self {
            bytes: 0,
            connection,
        }
    }

    fn reserve(&mut self, bytes: usize) -> bool {
        if self
            .connection
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(bytes)
                    .filter(|next| *next <= MAX_BASELINE_BYTES)
            })
            .is_err()
        {
            return false;
        }
        if RETAINED_BYTES
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(bytes)
                    .filter(|next| *next <= MAX_TOTAL_BYTES)
            })
            .is_err()
        {
            self.connection.fetch_sub(bytes, Ordering::Relaxed);
            return false;
        }
        self.bytes += bytes;
        true
    }

    fn release(&mut self, bytes: usize) {
        self.bytes -= bytes;
        self.connection.fetch_sub(bytes, Ordering::Relaxed);
        RETAINED_BYTES.fetch_sub(bytes, Ordering::Relaxed);
    }
}

impl Drop for Budget {
    fn drop(&mut self) {
        self.release(self.bytes);
    }
}

struct NativePatch {
    client_range: Range<usize>,
    output: Vec<Box<str>>,
    _budget: Budget,
}

/// 请求私有的恢复视图，只持有 client full 和发生重建的原生段
/// 相同前缀不另存第二份 full，多个恢复消费者共享同一已计费快照
pub(crate) struct NativeHistory {
    request: Box<str>,
    scope: Box<str>,
    patches: Vec<Arc<NativePatch>>,
    codex: bool,
    _budget: Budget,
}

impl NativeHistory {
    fn client(&self) -> Map<String, Value> {
        serde_json::from_str(&self.request).expect("validated client history")
    }

    fn apply(&self, input: &mut Vec<Value>) {
        for patch in self.patches.iter().rev() {
            input.splice(
                patch.client_range.clone(),
                patch.output.iter().map(|item| {
                    let mut item = serde_json::from_str(item).expect("validated native output");
                    // 快照保持原生，恢复发送和rebase比较共用原有Codex输入兼容视图
                    if self.codex {
                        crate::transport::downstream::normalize_reasoning_input_item(&mut item);
                    }
                    item
                }),
            );
        }
    }

    fn matches_recovery(&self, body: &Map<String, Value>, scope: &str) -> bool {
        if scope != self.scope.as_ref() {
            return false;
        }
        let mut full = self.client();
        if properties(body) != properties(&full) {
            return false;
        }
        let input = full
            .get_mut("input")
            .and_then(Value::as_array_mut)
            .expect("array client history");
        self.apply(input);
        body.get("input").and_then(Value::as_array) == Some(input)
    }

    pub(crate) fn restore(&self, body: &mut Value, scope: Option<&str>) {
        if scope != Some(self.scope.as_ref()) {
            return;
        }
        let mut full = self.client();
        let Some(current) = body.as_object() else {
            return;
        };
        // 请求私有令牌也须绑定原请求，克隆后编辑历史或参数不能复用它
        if current.get("input") != full.get("input") || properties(current) != properties(&full) {
            return;
        }
        let input = full
            .get_mut("input")
            .and_then(Value::as_array_mut)
            .expect("array client history");
        self.apply(input);
        body["input"] = Value::Array(std::mem::take(input));
    }
}

struct Baseline {
    history: Arc<NativeHistory>,
    output: Vec<Box<str>>,
    response_id: Box<str>,
    budget: Budget,
}

struct Pending {
    history: Arc<NativeHistory>,
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
    retained: Arc<AtomicUsize>,
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
    pub(crate) template: Option<&'static str>,
    pub(crate) history: Option<Arc<NativeHistory>>,
}

impl AutomaticDeltaPreparation {
    fn full(reason: &'static str) -> Self {
        Self {
            delta: None,
            reason,
            prefix_failure: None,
            template: None,
            history: None,
        }
    }

    fn prefix_failure(failure: PrefixFailure) -> Self {
        Self {
            delta: None,
            reason: "prefix_changed",
            prefix_failure: Some(failure),
            template: None,
            history: None,
        }
    }
}

impl AutomaticDeltaState {
    /// 消费上一轮基线并为本轮保留逻辑 full，不把已发送的 suffix 当成下轮历史
    pub(crate) fn prepare(
        &mut self,
        request: &crate::transport::protocol::responses::CodexResponsesRequest,
        full_payload: &str,
        latest_response_id: Option<&str>,
        codex: bool,
        enabled: bool,
    ) -> AutomaticDeltaPreparation {
        let body = request.body();
        let scope = request.automatic_delta_scope.as_deref().unwrap_or_default();
        let recovery = request.automatic_delta_recovery.get().cloned();
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
        if let Some(history) = recovery.filter(|history| history.matches_recovery(body, scope)) {
            self.retained = Arc::clone(&history._budget.connection);
            self.pending = Some(Pending {
                history: Arc::clone(&history),
                items: BTreeMap::new(),
                response_id: None,
                output_seen: false,
                max_output_index: None,
                budget: Budget::new(Arc::clone(&self.retained)),
            });
            let mut prepared = AutomaticDeltaPreparation::full("native_full_recovery");
            prepared.history = Some(history);
            return prepared;
        }
        let mut prepared = match previous.as_ref() {
            Some(previous) if previous.history.scope.as_ref() != scope => {
                AutomaticDeltaPreparation::full("credential_revision_changed")
            }
            Some(previous) if Some(previous.response_id.as_ref()) != latest_response_id => {
                AutomaticDeltaPreparation::full("owner_changed")
            }
            Some(previous) => compare(previous, body, input, codex),
            None => AutomaticDeltaPreparation::full("no_baseline"),
        };
        let mut patches = Vec::new();
        if let (Some(mut previous), Some((_, tail))) = (previous, prepared.delta.as_ref()) {
            patches = previous.history.patches.clone();
            let old_body: Value =
                serde_json::from_str(&previous.history.request).expect("validated prior request");
            let start = old_body["input"].as_array().expect("prior input").len();
            let end = input.len() - tail.len();
            // 已经完全相同的 raw 段无需再保存；已有映射仍继续共享
            let same = previous.output.len() == end - start
                && previous
                    .output
                    .iter()
                    .zip(&input[start..end])
                    .all(|(old, new)| {
                        serde_json::from_str::<Value>(old).ok().as_ref() == Some(new)
                    });
            let mapped_tail = tail != &input[end..];
            if !same || mapped_tail {
                previous.budget.release(previous.response_id.len());
                // 工具结果的 call_id 映射也进入原生恢复视图，client full 始终保持原值
                let mapped: Vec<Box<str>> = if mapped_tail {
                    tail.iter()
                        .map(|item| item.to_string().into_boxed_str())
                        .collect()
                } else {
                    Vec::new()
                };
                let extra = mapped.iter().map(|item| item.len()).sum::<usize>();
                let mut output = Vec::with_capacity(previous.output.len() + mapped.len());
                output.append(&mut previous.output);
                output.extend(mapped);
                if !previous.budget.reserve(
                    extra.saturating_add(
                        std::mem::size_of::<NativePatch>()
                            .saturating_add(output.capacity() * std::mem::size_of::<Box<str>>()),
                    ),
                ) {
                    return AutomaticDeltaPreparation::full("budget_exhausted");
                }
                patches.push(Arc::new(NativePatch {
                    client_range: start..if mapped_tail { input.len() } else { end },
                    output,
                    _budget: previous.budget,
                }));
            }
        }
        let mut budget = Budget::new(Arc::clone(&self.retained));
        let bytes = full_payload
            .len()
            .saturating_add(scope.len())
            .saturating_add(std::mem::size_of::<NativeHistory>())
            .saturating_add(patches.len() * std::mem::size_of::<Arc<NativePatch>>());
        if budget.reserve(bytes) {
            let history = Arc::new(NativeHistory {
                request: full_payload.into(),
                scope: scope.into(),
                patches,
                codex,
                _budget: budget,
            });
            if !history.patches.is_empty() {
                prepared.history = Some(Arc::clone(&history));
            }
            self.pending = Some(Pending {
                history,
                items: BTreeMap::new(),
                response_id: None,
                output_seen: false,
                max_output_index: None,
                budget: Budget::new(Arc::clone(&self.retained)),
            });
        } else {
            prepared = AutomaticDeltaPreparation::full("budget_exhausted");
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
                    history: pending.history,
                    output,
                    response_id,
                    budget: pending.budget,
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
    let Ok(previous_body) = serde_json::from_str::<Map<String, Value>>(&previous.history.request)
    else {
        return AutomaticDeltaPreparation::full("invalid_baseline");
    };
    if continuation_properties(&previous_body) != continuation_properties(body) {
        return AutomaticDeltaPreparation::full("properties_changed");
    }
    let Some(previous_input) = previous_body.get("input").and_then(Value::as_array) else {
        return AutomaticDeltaPreparation::full("invalid_baseline");
    };
    if input.len() < previous_input.len() {
        return AutomaticDeltaPreparation::prefix_failure(PrefixFailure::InputShorter);
    }
    if !previous_input
        .iter()
        .zip(input)
        .all(|(old, new)| items_match(old, new, codex))
    {
        return AutomaticDeltaPreparation::prefix_failure(
            if input.len() < previous_input.len().saturating_add(previous.output.len()) {
                PrefixFailure::InputShorter
            } else {
                PrefixFailure::PriorInputChanged
            },
        );
    }
    let Ok(output) = previous
        .output
        .iter()
        .map(|item| serde_json::from_str::<Value>(item))
        .collect::<Result<Vec<_>, _>>()
    else {
        return AutomaticDeltaPreparation::full("invalid_baseline");
    };
    let current = &input[previous_input.len()..];
    let strict_failure = if current.len() < output.len() {
        Some(PrefixFailure::InputShorter)
    } else {
        let mut failure = None;
        for (old, new) in output.iter().zip(current) {
            if items_match(old, new, codex) {
                continue;
            }
            if !output_metadata_matches(&project_item(old, codex), &project_item(new, codex)) {
                failure = Some(PrefixFailure::PriorOutputOther);
                break;
            }
            failure = Some(PrefixFailure::PriorOutputMetadataOnly);
        }
        failure
    };
    let (tail, template) = if let Some(failure) = strict_failure {
        let native_calls: Vec<String> = previous
            .history
            .patches
            .iter()
            .flat_map(|patch| &patch.output)
            .filter_map(|item| serde_json::from_str::<Value>(item).ok())
            .filter(|item| item["type"] == "function_call")
            .filter_map(|item| item["call_id"].as_str().map(str::to_owned))
            .collect();
        match projection::boundary(&output, current, previous_input, &native_calls) {
            Ok(claim) => (claim.tail, Some("turn_boundary")),
            Err(reason) => {
                let mut prepared = AutomaticDeltaPreparation::prefix_failure(failure);
                // 原四类诊断保持；受信拒绝单独记录原因
                prepared.template = Some(reason);
                return prepared;
            }
        }
    } else {
        (current[output.len()..].to_vec(), None)
    };
    AutomaticDeltaPreparation {
        delta: Some((previous.response_id.to_string(), tail)),
        reason: if template.is_some() {
            "trusted_delta"
        } else {
            "delta"
        },
        prefix_failure: None,
        template,
        history: None,
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

/// previous_response_id 引用历史，不继承本轮生成配置；这些参数仍由当前帧完整发送
/// model、store、截断/压缩及未知属性变化仍保守建立新链，恢复令牌另用完整参数校验
fn continuation_properties(body: &Map<String, Value>) -> Map<String, Value> {
    let mut result = properties(body);
    for key in [
        "instructions",
        "tools",
        "tool_choice",
        "parallel_tool_calls",
        "reasoning",
        "text",
        "include",
        "service_tier",
        "prompt_cache_key",
        "metadata",
        "max_output_tokens",
        "max_tool_calls",
        "temperature",
        "top_p",
        "user",
        "safety_identifier",
        "prompt_cache_retention",
    ] {
        result.shift_remove(key);
    }
    result
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
