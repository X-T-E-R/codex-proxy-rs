//! 模型级请求策略值对象：按客户端请求模型精确匹配，冻结进路由计划后由 Provider 应用。

/// 规范化推理强度；声明顺序即强度从低到高。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl ReasoningEffort {
    /// 解析请求或配置中的强度值；大小写与首尾空白不敏感。
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" => Some(Self::None),
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::XHigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// 推理强度策略模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReasoningEffortRuleMode {
    /// 无条件改写为指定强度。
    Locked,
    /// 请求缺失或低于指定强度时改写。
    Min,
    /// 请求缺失或高于指定强度时改写。
    Max,
}

impl ReasoningEffortRuleMode {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "locked" => Some(Self::Locked),
            "min" => Some(Self::Min),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Locked => "locked",
            Self::Min => "min",
            Self::Max => "max",
        }
    }
}

/// 请求当前携带的推理强度；`Unknown` 是无法识别的自定义值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestedReasoningEffort {
    Absent,
    Unknown,
    Known(ReasoningEffort),
}

/// 单条推理强度策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReasoningEffortRule {
    mode: ReasoningEffortRuleMode,
    value: ReasoningEffort,
}

impl ReasoningEffortRule {
    #[must_use]
    pub const fn new(mode: ReasoningEffortRuleMode, value: ReasoningEffort) -> Self {
        Self { mode, value }
    }

    #[must_use]
    pub const fn mode(self) -> ReasoningEffortRuleMode {
        self.mode
    }

    #[must_use]
    pub const fn value(self) -> ReasoningEffort {
        self.value
    }

    /// 计算请求应改写成的最终强度；`None` 表示保持请求原值。
    /// 无法识别的请求值在 min/max 下保持不动，避免破坏未来的协议取值。
    #[must_use]
    pub fn resolve(self, requested: RequestedReasoningEffort) -> Option<ReasoningEffort> {
        match (self.mode, requested) {
            (ReasoningEffortRuleMode::Locked, _) => Some(self.value),
            (ReasoningEffortRuleMode::Min, RequestedReasoningEffort::Absent) => Some(self.value),
            (ReasoningEffortRuleMode::Max, RequestedReasoningEffort::Absent) => Some(self.value),
            (ReasoningEffortRuleMode::Min, RequestedReasoningEffort::Known(current))
                if current < self.value =>
            {
                Some(self.value)
            }
            (ReasoningEffortRuleMode::Max, RequestedReasoningEffort::Known(current))
                if current > self.value =>
            {
                Some(self.value)
            }
            _ => None,
        }
    }
}

/// 服务档位策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServiceTierRule {
    /// 无条件改写为 fast。
    LockFast,
    /// 请求携带 fast/priority 时移除档位字段，按默认档处理。
    LockNeverFast,
}

impl ServiceTierRule {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "lock_fast" => Some(Self::LockFast),
            "lock_never_fast" => Some(Self::LockNeverFast),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LockFast => "lock_fast",
            Self::LockNeverFast => "lock_never_fast",
        }
    }
}

/// 一个客户端请求模型的完整请求策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ModelRequestPolicy {
    reasoning_effort: Option<ReasoningEffortRule>,
    service_tier: Option<ServiceTierRule>,
}

impl ModelRequestPolicy {
    /// 两项都为空时不存在策略。
    #[must_use]
    pub const fn new(
        reasoning_effort: Option<ReasoningEffortRule>,
        service_tier: Option<ServiceTierRule>,
    ) -> Option<Self> {
        if reasoning_effort.is_none() && service_tier.is_none() {
            return None;
        }
        Some(Self {
            reasoning_effort,
            service_tier,
        })
    }

    /// 存储或配置读出的未校验策略；任一字段非法时不存在策略。
    #[must_use]
    pub fn from_facts(
        reasoning_effort_mode: Option<&str>,
        reasoning_effort_value: Option<&str>,
        service_tier: Option<&str>,
    ) -> Option<Self> {
        let reasoning_effort = match (reasoning_effort_mode, reasoning_effort_value) {
            (Some(mode), Some(value)) => Some(ReasoningEffortRule::new(
                ReasoningEffortRuleMode::parse(mode)?,
                ReasoningEffort::parse(value)?,
            )),
            (None, None) => None,
            _ => return None,
        };
        let service_tier = match service_tier {
            Some(tier) => Some(ServiceTierRule::parse(tier)?),
            None => None,
        };
        Self::new(reasoning_effort, service_tier)
    }

    #[must_use]
    pub const fn reasoning_effort(self) -> Option<ReasoningEffortRule> {
        self.reasoning_effort
    }

    #[must_use]
    pub const fn service_tier(self) -> Option<ServiceTierRule> {
        self.service_tier
    }
}
