-- 请求级原值按 request/attempt 单独收集，避免异步执行观测与账号观测队列的写入顺序产生误归属。
alter table model_requests
  add column turn_state_collection_enabled boolean not null default false;

create table request_turn_state_observations (
  request_id text not null,
  attempt_index integer not null check (attempt_index > 0),
  observation_id text not null,
  value text not null check (octet_length(value) <= 16384),
  observed_at timestamptz not null,
  source text not null check (source in ('http', 'websocket')),
  upstream_response_id text,
  changed boolean not null default false,
  primary key (request_id, attempt_index)
);
create index request_turn_state_observations_observed_at_idx
  on request_turn_state_observations (observed_at);
