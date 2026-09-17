-- 普通 token refresh 只推进 credential_revision；模型锁按独立身份代次隔离。
alter table provider_accounts
  add column identity_revision bigint not null default 1 check (identity_revision > 0);

-- 模型级 Turn State 锁定按账号身份代次与实际上游模型隔离；捕获任务本身只驻留进程内。
create table openai_model_turn_states (
  account_id text not null references provider_accounts(id) on delete cascade,
  identity_revision bigint not null check (identity_revision > 0),
  effective_model text not null check (nullif(effective_model, '') is not null and octet_length(effective_model) <= 512),
  lock_enabled boolean not null default false,
  capture_enabled boolean not null default false,
  reuse_window_seconds integer not null default 3600 check (reuse_window_seconds between 1 and 86400),
  capture_proxy_id text references outbound_proxies(id) on delete restrict,
  max_attempts smallint not null default 3 check (max_attempts between 1 and 10),
  attempt_timeout_seconds smallint not null default 8 check (attempt_timeout_seconds between 1 and 60),
  job_timeout_seconds smallint not null default 30 check (job_timeout_seconds between 1 and 300),
  backoff_seconds smallint not null default 1 check (backoff_seconds between 0 and 60),
  max_backoff_seconds smallint not null default 4 check (max_backoff_seconds between 0 and 60),
  cooldown_seconds integer not null default 900 check (cooldown_seconds between 0 and 86400),
  pin_value text,
  pin_token_version smallint,
  pin_issued_at timestamptz,
  pin_raw_bytes integer,
  pin_source text check (pin_source is null or pin_source in ('manual', 'capture')),
  pin_compatible_transport text check (pin_compatible_transport is null or pin_compatible_transport = 'http'),
  pin_captured_at timestamptz,
  pin_reuse_deadline timestamptz,
  pin_invalidated_at timestamptz,
  config_revision bigint not null default 1 check (config_revision > 0),
  updated_at timestamptz not null default now(),
  primary key (account_id, identity_revision, effective_model),
  constraint openai_model_turn_states_pin_ck check (
    (pin_value is null and pin_token_version is null and pin_issued_at is null
      and pin_raw_bytes is null and pin_source is null and pin_compatible_transport is null
      and pin_captured_at is null and pin_reuse_deadline is null and pin_invalidated_at is null)
    or
    (octet_length(pin_value) = 292 and pin_value ~ '^[ -~]+$'
      and pin_source is not null and pin_compatible_transport = 'http'
      and pin_captured_at is not null and pin_reuse_deadline is not null)
  ),
  constraint openai_model_turn_states_token_metadata_ck check (
    (pin_token_version is null and pin_issued_at is null and pin_raw_bytes is null)
    or
    (pin_token_version between 0 and 255 and pin_raw_bytes > 0)
  ),
  constraint openai_model_turn_states_lock_ck check (not lock_enabled or pin_value is not null),
  constraint openai_model_turn_states_capture_proxy_ck check (not capture_enabled or capture_proxy_id is not null),
  constraint openai_model_turn_states_timeout_ck check (attempt_timeout_seconds <= job_timeout_seconds),
  constraint openai_model_turn_states_backoff_ck check (backoff_seconds <= max_backoff_seconds)
);

create index openai_model_turn_states_updated_at_idx on openai_model_turn_states (updated_at);
