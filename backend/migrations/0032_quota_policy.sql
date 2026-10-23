-- 自定义暂停独立于 enabled 与上游 quota；所有新规则默认关闭。
create table codex_quota_policy_global (
  id integer primary key check (id = 1),
  revision bigint not null default 0 check (revision >= 0),
  policy_json jsonb not null,
  updated_at timestamptz not null default now()
);
insert into codex_quota_policy_global(id, policy_json) values (1,
  '{"primary":{"action":"off","thresholdPercent":100},"secondary":{"action":"off","thresholdPercent":100},"autoReset":{"maxAttemptsPer24h":1,"cooldownSeconds":86400}}');

create table codex_quota_policy_accounts (
  account_id text primary key references provider_accounts(id) on delete cascade,
  revision bigint not null default 0 check (revision >= 0),
  mode text not null default 'inherit' check (mode in ('inherit','disabled','custom')),
  policy_json jsonb,
  paused boolean not null default false,
  reason text not null default 'off',
  observed_at timestamptz,
  updated_at timestamptz not null default now(),
  check ((mode = 'custom') = (policy_json is not null))
);

-- 已发送但未知也占预算；窗口/凭据/配置变化均不删除历史。
create table codex_reset_operations (
  id text primary key check (id ~ '^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$'),
  account_id text not null references provider_accounts(id) on delete cascade,
  credential_revision bigint not null,
  credit_id text,
  automatic boolean not null,
  episode text,
  trigger_percent smallint check (trigger_percent between 1 and 100),
  state text not null check (state in ('pending','confirmed','failed')),
  needs_readback boolean not null default false,
  result_code text,
  sent_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  check (not automatic or (episode is not null and trigger_percent is not null)),
  check (not needs_readback or (automatic and state = 'confirmed'))
);
create unique index codex_reset_one_pending on codex_reset_operations(account_id) where state = 'pending' or needs_readback;
create unique index codex_reset_one_episode on codex_reset_operations(account_id, episode) where automatic;
create index codex_reset_budget on codex_reset_operations(account_id, sent_at) where automatic;

-- 仅记录脱敏策略修改来源，不保存凭据或上游正文。
create table codex_quota_policy_audit (
  id bigserial primary key,
  account_id text,
  revision bigint not null,
  actor_ref text not null,
  created_at timestamptz not null default now()
);
