-- 锁定与捕获策略属于账号身份；具体密文仍按实际上游模型隔离。
create table openai_account_turn_state_policies (
  account_id text not null references provider_accounts(id) on delete cascade,
  identity_revision bigint not null check (identity_revision > 0),
  lock_enabled boolean not null default false,
  capture_enabled boolean not null default false,
  reuse_window_seconds integer not null default 7200 check (reuse_window_seconds between 1 and 86400),
  capture_proxy_id text references outbound_proxies(id) on delete restrict,
  max_attempts smallint not null default 3 check (max_attempts between 1 and 10),
  attempt_timeout_seconds smallint not null default 8 check (attempt_timeout_seconds between 1 and 60),
  job_timeout_seconds smallint not null default 30 check (job_timeout_seconds between 1 and 300),
  backoff_seconds smallint not null default 1 check (backoff_seconds between 0 and 60),
  max_backoff_seconds smallint not null default 4 check (max_backoff_seconds between 0 and 60),
  cooldown_seconds integer not null default 900 check (cooldown_seconds between 0 and 86400),
  config_revision bigint not null default 1 check (config_revision > 0),
  updated_at timestamptz not null default now(),
  primary key (account_id, identity_revision)
);

-- 已部署的逐模型策略收敛为一个账号策略：任一模型启用即视为账号启用；
-- 其余冲突字段取最新模型行，代理取最新的非空值，模型名作为稳定平局键。
insert into openai_account_turn_state_policies (
  account_id, identity_revision, lock_enabled, capture_enabled,
  reuse_window_seconds, capture_proxy_id, max_attempts,
  attempt_timeout_seconds, job_timeout_seconds, backoff_seconds,
  max_backoff_seconds, cooldown_seconds, config_revision, updated_at
)
select account_id,
       identity_revision,
       bool_or(lock_enabled),
       bool_or(capture_enabled),
       (array_agg(reuse_window_seconds order by updated_at desc, effective_model asc))[1],
       (array_agg(capture_proxy_id order by (capture_proxy_id is not null) desc,
                  updated_at desc, effective_model asc))[1],
       (array_agg(max_attempts order by updated_at desc, effective_model asc))[1],
       (array_agg(attempt_timeout_seconds order by updated_at desc, effective_model asc))[1],
       (array_agg(job_timeout_seconds order by updated_at desc, effective_model asc))[1],
       (array_agg(backoff_seconds order by updated_at desc, effective_model asc))[1],
       (array_agg(max_backoff_seconds order by updated_at desc, effective_model asc))[1],
       (array_agg(cooldown_seconds order by updated_at desc, effective_model asc))[1],
       greatest(max(config_revision), 1),
       max(updated_at)
  from openai_model_turn_states
 group by account_id, identity_revision;

-- 新账号或尚未打开过模型面板的账号也有一个可直接编辑的默认策略。
insert into openai_account_turn_state_policies(account_id, identity_revision)
select id, identity_revision
  from provider_accounts
 where provider_kind = 'openai'
on conflict do nothing;

-- 旧表中的开关仅保留迁移来源，不再参与运行时约束或决策。
alter table openai_model_turn_states
  drop constraint openai_model_turn_states_lock_ck,
  drop constraint openai_model_turn_states_capture_proxy_ck;

-- 策略迁移完成后移除逐模型副本及其旧代理外键，避免已不生效的模型行继续阻止代理删除。
alter table openai_model_turn_states
  drop constraint openai_model_turn_states_capture_proxy_id_fkey,
  drop column lock_enabled,
  drop column capture_enabled,
  drop column reuse_window_seconds,
  drop column capture_proxy_id,
  drop column max_attempts,
  drop column attempt_timeout_seconds,
  drop column job_timeout_seconds,
  drop column backoff_seconds,
  drop column max_backoff_seconds,
  drop column cooldown_seconds;
