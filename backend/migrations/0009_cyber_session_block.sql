alter table runtime_settings
    add column cyber_session_block_enabled boolean not null default false,
    add column cyber_session_block_ttl_seconds bigint not null default 3600,
    add constraint runtime_settings_cyber_session_block_ttl_positive
        check (cyber_session_block_ttl_seconds > 0 and cyber_session_block_ttl_seconds <= 4294967295);
