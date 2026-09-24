-- 账号级过载冷号覆盖：默认跟随全局设置，可关闭或使用账号自有阈值。
alter table provider_accounts
    add column overload_cooldown_mode text not null default 'inherit',
    add column overload_cooldown_threshold bigint,
    add column overload_cooldown_seconds bigint,
    add constraint provider_accounts_overload_cooldown_mode_ck check (
        overload_cooldown_mode in ('inherit', 'disabled', 'custom')
    ),
    add constraint provider_accounts_overload_cooldown_custom_ck check (
        (
            overload_cooldown_mode = 'custom'
            and overload_cooldown_threshold is not null
            and overload_cooldown_seconds is not null
            and overload_cooldown_threshold between 1 and 4294967295
            and overload_cooldown_seconds between 1 and 4294967295
        ) or (
            overload_cooldown_mode <> 'custom'
            and overload_cooldown_threshold is null
            and overload_cooldown_seconds is null
        )
    );
