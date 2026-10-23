-- OpenAI 上游 WebSocket 连接池的运行参数：随 runtime_settings 单例原子替换，
-- 经 config_revision 传播后无需重启生效。列默认值与代码默认值保持一致，
-- 全默认数据下的运行语义与引入本设置前等价。
alter table runtime_settings
  add column ws_pool_enabled boolean not null default true,
  add column ws_pool_max_age_ms bigint not null default 3300000,
  add column ws_pool_max_connecting bigint not null default 8,
  add column ws_pool_stream_idle_timeout_ms bigint not null default 300000,
  add column ws_pool_fast_path_budget_ms bigint not null default 800,
  add constraint runtime_settings_ws_pool_ck check (
    ws_pool_max_age_ms > 0
    and ws_pool_max_connecting > 0
    and ws_pool_stream_idle_timeout_ms > 0
    and ws_pool_fast_path_budget_ms > 0
  );
