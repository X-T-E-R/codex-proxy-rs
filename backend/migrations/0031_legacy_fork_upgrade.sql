-- Preserve the fork's deployed account allowlists when adopting the upstream
-- all/allowlist/denylist document.
update provider_accounts
set model_access_json = jsonb_build_object(
    'mode', 'allowlist',
    'models', to_jsonb(allowed_models)
)
where allowed_models is not null
  and model_access_json = '{"mode":"all","models":[]}'::jsonb;

-- The fork's earlier queue had no waiter-count limit and used a 60-second
-- timeout by default. Map that behavior to the bounded upstream queue instead
-- of silently disabling waiting after upgrade.
update runtime_settings
set max_waiting_per_key = 1000,
    max_waiting_per_account = 1000,
    concurrency_wait_timeout_seconds = greatest(
        1,
        least(120, capacity_queue_timeout_seconds)
    )
where max_waiting_per_key = 0
  and max_waiting_per_account = 0;
