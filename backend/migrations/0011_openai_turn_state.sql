-- 账号级手动出站覆盖与真实上游最近一次观测独立保存。
create table openai_turn_states (
  account_id text primary key references provider_accounts(id) on delete cascade,
  enabled boolean not null default false,
  override_value text,
  override_updated_at timestamptz,
  config_revision bigint not null default 1,
  observed_id text,
  observed_value text,
  observed_at timestamptz,
  observed_transport text,
  observed_upstream_response_id text,
  observed_client_turn_id text,
  constraint openai_turn_states_revision_ck check (config_revision > 0),
  constraint openai_turn_states_enabled_ck check (not enabled or nullif(override_value, '') is not null),
  constraint openai_turn_states_override_size_ck check (override_value is null or octet_length(override_value) <= 16384),
  constraint openai_turn_states_override_header_ck check (
    override_value is null or override_value ~ '^[ -~]+$'
  ),
  constraint openai_turn_states_observed_size_ck check (observed_value is null or octet_length(observed_value) <= 16384)
);
