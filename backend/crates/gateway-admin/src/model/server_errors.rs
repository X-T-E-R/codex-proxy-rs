//! 服务端错误族的请求去重计数与非完整尝试观测

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerErrorCounts {
    pub retained_requests: u64,
    pub server_related_requests: u64,
    pub overloaded: u64,
    pub status503: u64,
    pub other_server_prefix: u64,
    pub other5xx: u64,
    pub upstream503: u64,
    pub infrastructure503: u64,
    pub no_provider503: u64,
    pub unclassified503: u64,
    pub client200_errors: u64,
    pub legacy_message_matches: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerErrorDay {
    pub day: String,
    #[serde(flatten)]
    pub counts: ServerErrorCounts,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerErrorAttempts {
    pub observed: u64,
    pub server_related: u64,
    pub overloaded: u64,
    pub status503: u64,
    pub coverage: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerErrorSummary {
    pub from_inclusive: DateTime<Utc>,
    pub to_exclusive: DateTime<Utc>,
    pub timezone: String,
    pub time_basis: String,
    pub source: String,
    pub earliest_retained_at: Option<DateTime<Utc>>,
    pub latest_retained_at: Option<DateTime<Utc>>,
    pub totals: ServerErrorCounts,
    pub daily: Vec<ServerErrorDay>,
    pub attempts: ServerErrorAttempts,
}
