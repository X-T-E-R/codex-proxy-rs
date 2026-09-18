-- Turn State 统一为账号身份 + 实际模型的一份 active 与一份候选值。
alter table openai_account_turn_state_policies
  add column refresh_lead_seconds integer not null default 900
    check (refresh_lead_seconds between 0 and 86400);

alter table openai_model_turn_states
  drop constraint openai_model_turn_states_pin_ck,
  add column active_generation bigint not null default 1 check (active_generation > 0),
  add column active_activated_at timestamptz,
  add column active_candidate_id text,
  add column candidate_id text,
  add column candidate_value text,
  add column candidate_token_version smallint,
  add column candidate_issued_at timestamptz,
  add column candidate_raw_bytes integer,
  add column candidate_source text,
  add column candidate_captured_at timestamptz,
  add column candidate_reuse_deadline timestamptz,
  add column capture_not_before timestamptz,
  add column capture_last_result text,
  add column capture_last_finished_at timestamptz,
  add column rejected_value_sha256 text;

update openai_model_turn_states
   set active_activated_at = pin_captured_at
 where pin_value is not null;

alter table openai_model_turn_states
  add constraint openai_model_turn_states_active_ck check (
    (pin_value is null and pin_token_version is null and pin_issued_at is null
      and pin_raw_bytes is null and pin_source is null and pin_compatible_transport is null
      and pin_captured_at is null and pin_reuse_deadline is null and pin_invalidated_at is null
      and active_activated_at is null and active_candidate_id is null)
    or
    (octet_length(pin_value) between 1 and 16384 and pin_value ~ '^[ -~]+$'
      and pin_source is not null and pin_compatible_transport = 'http'
      and pin_captured_at is not null and pin_reuse_deadline is not null
      and active_activated_at is not null)
  ),
  add constraint openai_model_turn_states_candidate_ck check (
    (candidate_id is null and candidate_value is null and candidate_token_version is null
      and candidate_issued_at is null and candidate_raw_bytes is null
      and candidate_source is null and candidate_captured_at is null
      and candidate_reuse_deadline is null)
    or
    (nullif(candidate_id, '') is not null and octet_length(candidate_id) <= 128
      and octet_length(candidate_value) = 292 and candidate_value ~ '^[ -~]+$'
      and candidate_source in ('capture', 'observation')
      and candidate_captured_at is not null and candidate_reuse_deadline is not null)
  ),
  add constraint openai_model_turn_states_candidate_metadata_ck check (
    (candidate_token_version is null and candidate_issued_at is null and candidate_raw_bytes is null)
    or (candidate_token_version between 0 and 255 and candidate_raw_bytes > 0)
  ),
  add constraint openai_model_turn_states_active_candidate_id_ck check (
    active_candidate_id is null
    or (nullif(active_candidate_id, '') is not null and octet_length(active_candidate_id) <= 128)
  ),
  add constraint openai_model_turn_states_rejected_sha_ck check (
    rejected_value_sha256 is null or rejected_value_sha256 ~ '^[0-9a-f]{64}$'
  );

create index openai_model_turn_states_capture_schedule_idx
  on openai_model_turn_states (capture_not_before, account_id, identity_revision, effective_model)
  where capture_requested_at is not null;

-- 同一 attempt 分别保存实际发送值与上游返回值；历史行没有 sent 证据。
alter table request_turn_state_observations
  alter column observation_id drop not null,
  alter column value drop not null,
  alter column observed_at drop not null,
  alter column source drop not null,
  add column sent_value text check (sent_value is null or octet_length(sent_value) <= 16384),
  add column sent_at timestamptz,
  add column sent_source text,
  add column sent_transport text,
  add column sent_account_id text,
  add column sent_identity_revision bigint,
  add column sent_effective_model text,
  add column sent_generation bigint,
  add column sent_candidate_id text,
  add constraint request_turn_state_sent_ck check (
    (sent_value is null and sent_at is null and sent_source is null and sent_transport is null
      and sent_account_id is null and sent_identity_revision is null
      and sent_effective_model is null and sent_generation is null
      and sent_candidate_id is null)
    or
    (sent_value is not null and sent_at is not null
      and sent_source in ('manual', 'capture', 'observation', 'continuation', 'client_passthrough')
      and sent_transport in ('http', 'websocket')
      and sent_account_id is not null and sent_identity_revision > 0
      and sent_effective_model is not null)
  );

alter table model_requests
  add column turn_state_sent_collection_enabled boolean not null default false;
