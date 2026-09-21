-- 过载冷号默认关闭；设置发布到请求级调度快照，冷却期限存放于 Redis。
alter table runtime_settings
    add column overload_cooldown_enabled boolean not null default false,
    add column overload_cooldown_threshold bigint not null default 2
        check (overload_cooldown_threshold between 1 and 4294967295),
    add column overload_cooldown_seconds bigint not null default 120
        check (overload_cooldown_seconds between 1 and 4294967295);
