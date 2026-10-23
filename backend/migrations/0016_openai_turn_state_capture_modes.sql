-- 自动捕获触发语义与 active/candidate 实际发送使用记录。
alter table openai_account_turn_state_policies
  add column capture_trigger_mode text not null default 'on_attributed_failure'
    check (capture_trigger_mode in (
      'before_expiry_if_used',
      'on_attributed_failure',
      'first_request_after_expiry',
      'failure_or_first_after_expiry'
    ));

alter table openai_model_turn_states
  add column initial_observation_scope_id text
    check (initial_observation_scope_id is null
      or (nullif(initial_observation_scope_id, '') is not null
        and octet_length(initial_observation_scope_id) <= 128)),
  add column active_sent_count bigint not null default 0 check (active_sent_count >= 0),
  add column active_last_sent_at timestamptz,
  add column candidate_sent_count bigint not null default 0 check (candidate_sent_count >= 0),
  add column candidate_last_sent_at timestamptz,
  add constraint openai_model_turn_states_active_sent_ck check (
    (pin_value is null and active_sent_count = 0 and active_last_sent_at is null)
    or
    (pin_value is not null and (
      (active_sent_count = 0 and active_last_sent_at is null)
      or (active_sent_count > 0 and active_last_sent_at is not null)
    ))
  ),
  add constraint openai_model_turn_states_candidate_sent_ck check (
    (candidate_value is null and candidate_sent_count = 0 and candidate_last_sent_at is null)
    or
    (candidate_value is not null and (
      (candidate_sent_count = 0 and candidate_last_sent_at is null)
      or (candidate_sent_count > 0 and candidate_last_sent_at is not null)
    ))
  );

-- 已发布的旧列继续承担存储完整性，但值改为 Responses transport 家族，
-- 对外 API 使用 compatibleTransports 明确列出 HTTP 与 WebSocket。
alter table openai_model_turn_states
  drop constraint openai_model_turn_states_active_ck,
  drop constraint openai_model_turn_states_pin_compatible_transport_check;

update openai_model_turn_states
   set pin_compatible_transport = 'responses'
 where pin_value is not null;

alter table openai_model_turn_states
  add constraint openai_model_turn_states_pin_compatible_transport_check check (
    pin_compatible_transport is null or pin_compatible_transport = 'responses'
  ),
  add constraint openai_model_turn_states_active_ck check (
    (pin_value is null and pin_token_version is null and pin_issued_at is null
      and pin_raw_bytes is null and pin_source is null and pin_compatible_transport is null
      and pin_captured_at is null and pin_reuse_deadline is null and pin_invalidated_at is null
      and active_activated_at is null and active_candidate_id is null)
    or
    (octet_length(pin_value) between 1 and 16384 and pin_value ~ '^[ -~]+$'
      and pin_source is not null and pin_compatible_transport = 'responses'
      and pin_captured_at is not null and pin_reuse_deadline is not null
      and active_activated_at is not null)
  );
