//! 有界历史删除与诊断清除，不承载周期、暂停或重试策略

use std::num::NonZeroU32;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::store::AdminStoreResult;
use crate::model::retention::{RetentionPolicy, RetentionTarget};

#[async_trait]
pub trait RetentionStore: Send + Sync {
    async fn load_policy(&self) -> AdminStoreResult<RetentionPolicy>;

    /// 每批最多处理 limit 行，返回该 target 的受影响行数
    /// 请求删除级联清理关联事件；成功诊断 target 只清除 trace，保留请求
    async fn purge_batch(
        &self,
        target: RetentionTarget,
        now: DateTime<Utc>,
        policy: RetentionPolicy,
        limit: NonZeroU32,
    ) -> AdminStoreResult<u64>;
}
