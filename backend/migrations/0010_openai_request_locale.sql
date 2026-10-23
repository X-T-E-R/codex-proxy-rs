alter table runtime_settings
    add column openai_request_body_override_enabled boolean not null default true,
    add column openai_request_timezone text not null default 'America/Los_Angeles',
    add column openai_search_country text not null default 'US',
    add constraint runtime_settings_openai_request_timezone_valid
        check (length(btrim(openai_request_timezone)) > 0
            and octet_length(openai_request_timezone) <= 128
            and openai_request_timezone !~ '[^ -~]'),
    add constraint runtime_settings_openai_search_country_valid
        check (openai_search_country ~ '^[A-Z]{2}$');
