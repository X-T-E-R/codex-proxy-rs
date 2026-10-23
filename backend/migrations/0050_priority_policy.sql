-- 独立于分组策略和凭据事实保存全局/账号快速模式与配额代次
create table codex_priority_global (
    id smallint primary key check (id=1),
    revision bigint not null default 0 check (revision >= 0),
    mode text not null default 'default' check (mode in ('default','enabled','disabled')),
    updated_at timestamptz not null default now()
);
insert into codex_priority_global(id) values(1);

create table codex_priority_accounts (
    account_id text primary key references provider_accounts(id) on delete cascade,
    revision bigint not null default 0 check (revision >= 0),
    mode text not null default 'inherit' check (mode in ('inherit','enabled','disabled')),
    auto_off_on_quota_reset boolean not null default false,
    baseline jsonb,
    last_auto_off_at timestamptz,
    reason text not null default 'off',
    updated_at timestamptz not null default now()
);

create table codex_priority_audit (
    id bigint generated always as identity primary key,
    account_id text,
    revision bigint not null,
    actor_ref text not null,
    details jsonb not null,
    created_at timestamptz not null default now()
);
create index codex_priority_audit_account on codex_priority_audit(account_id,created_at desc);
