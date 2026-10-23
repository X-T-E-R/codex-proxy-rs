//! 会话保护和 WebSocket 池的持久运行值与范围校验

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeProtectionSettings {
    pub cyber_session_block_enabled: bool,
    pub cyber_session_block_ttl_seconds: u32,
    pub ws_pool_enabled: bool,
    pub ws_pool_max_age_ms: u64,
    pub ws_pool_max_connecting: u32,
    pub ws_pool_stream_idle_timeout_ms: u64,
    pub ws_pool_fast_path_budget_ms: u64,
}

impl Default for RuntimeProtectionSettings {
    fn default() -> Self {
        Self {
            cyber_session_block_enabled: false,
            cyber_session_block_ttl_seconds: 3600,
            ws_pool_enabled: true,
            ws_pool_max_age_ms: 3_300_000,
            ws_pool_max_connecting: 8,
            ws_pool_stream_idle_timeout_ms: 300_000,
            ws_pool_fast_path_budget_ms: 800,
        }
    }
}

impl RuntimeProtectionSettings {
    pub fn validate(self) -> Result<(), &'static str> {
        if self.cyber_session_block_ttl_seconds == 0 {
            return Err("cyber_session_block_ttl_seconds");
        }
        for (value, field) in [
            (self.ws_pool_max_age_ms, "ws_pool_max_age_ms"),
            (
                self.ws_pool_stream_idle_timeout_ms,
                "ws_pool_stream_idle_timeout_ms",
            ),
            (
                self.ws_pool_fast_path_budget_ms,
                "ws_pool_fast_path_budget_ms",
            ),
        ] {
            if value == 0 || i64::try_from(value).is_err() {
                return Err(field);
            }
        }
        if self.ws_pool_max_connecting == 0 {
            return Err("ws_pool_max_connecting");
        }
        Ok(())
    }

    #[must_use]
    pub fn cyber_ttl(self) -> Option<std::time::Duration> {
        self.cyber_session_block_enabled.then(|| {
            std::time::Duration::from_secs(u64::from(self.cyber_session_block_ttl_seconds))
        })
    }
}
