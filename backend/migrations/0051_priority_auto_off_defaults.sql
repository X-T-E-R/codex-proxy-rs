-- 自动关闭可由全局提供默认，账号 null 继承，显式 false 保持覆盖
alter table codex_priority_global add column auto_off_on_quota_reset boolean not null default false;
alter table codex_priority_accounts alter column auto_off_on_quota_reset drop not null;
alter table codex_priority_accounts alter column auto_off_on_quota_reset drop default;
alter table codex_priority_accounts add column baseline_global_revision bigint;
