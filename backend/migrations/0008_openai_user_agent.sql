-- NULL 沿用启动画像；管理端可覆盖上游 User-Agent，重启后仍生效。
alter table runtime_settings
    add column openai_user_agent text
        check (openai_user_agent is null or
            (length(btrim(openai_user_agent)) > 0
             and octet_length(openai_user_agent) <= 512
             and openai_user_agent !~ '[^ -~]'));
