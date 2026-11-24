//! 账号对全局可选保护的显式关闭
//!
//! 缺省继承全局开关。只记录被关闭的功能，不覆盖阈值，也不表达自定义策略

use std::collections::BTreeSet;

/// 可以按单个上游账号关闭的可选功能
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AccountOptionalFeature {
    /// 过载保护 / 容量熔断
    CapacityFreeze,
    /// 账号预热
    AccountWarmup,
    /// cyber 会话自动屏蔽
    CyberSessionBlock,
}

impl AccountOptionalFeature {
    pub const ALL: [Self; 3] = [
        Self::CapacityFreeze,
        Self::AccountWarmup,
        Self::CyberSessionBlock,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CapacityFreeze => "capacity_freeze",
            Self::AccountWarmup => "account_warmup",
            Self::CyberSessionBlock => "cyber_session_block",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "capacity_freeze" => Some(Self::CapacityFreeze),
            "account_warmup" => Some(Self::AccountWarmup),
            "cyber_session_block" => Some(Self::CyberSessionBlock),
            _ => None,
        }
    }
}

/// 一个账号显式关闭的可选功能；空集表示全部继承全局
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountFeatureOptOuts {
    disabled: BTreeSet<AccountOptionalFeature>,
}

impl Default for AccountFeatureOptOuts {
    fn default() -> Self {
        Self::empty()
    }
}

impl AccountFeatureOptOuts {
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            disabled: BTreeSet::new(),
        }
    }

    #[must_use]
    pub fn new(features: impl IntoIterator<Item = AccountOptionalFeature>) -> Self {
        Self {
            disabled: features.into_iter().collect(),
        }
    }

    /// 只认已知功能标记，其余持久化文本留给原列继续保存
    #[must_use]
    pub fn from_persisted_tokens(tokens: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        Self::new(
            tokens
                .into_iter()
                .filter_map(|token| AccountOptionalFeature::parse(token.as_ref())),
        )
    }

    /// 管理端提交的完整关闭列表；未知或重复标记拒绝
    #[allow(clippy::result_unit_err)]
    pub fn from_wire_tokens(tokens: &[String]) -> Result<Self, ()> {
        let mut disabled = BTreeSet::new();
        for token in tokens {
            let Some(feature) = AccountOptionalFeature::parse(token) else {
                return Err(());
            };
            if !disabled.insert(feature) {
                return Err(());
            }
        }
        Ok(Self { disabled })
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.disabled.is_empty()
    }

    #[must_use]
    pub fn contains(&self, feature: AccountOptionalFeature) -> bool {
        self.disabled.contains(&feature)
    }

    pub fn iter(&self) -> impl Iterator<Item = AccountOptionalFeature> + '_ {
        self.disabled.iter().copied()
    }

    #[must_use]
    pub fn tokens(&self) -> Vec<String> {
        self.disabled
            .iter()
            .map(|feature| feature.as_str().to_owned())
            .collect()
    }
}
