-- 账号模型白名单默认放行全部模型；并发池满时按运行设置进行有界等待。
alter table provider_accounts
  add column allowed_models text[],
  add constraint provider_accounts_allowed_models_ck check (
    allowed_models is null
    or (
      cardinality(allowed_models) between 1 and 512
      and array_position(allowed_models, null) is null
      and array_position(allowed_models, '') is null
      and octet_length(array_to_string(allowed_models, chr(31))) <= 131072
    )
  );

alter table runtime_settings
  add column capacity_queue_retry_seconds bigint not null default 3,
  add column capacity_queue_timeout_seconds bigint not null default 60,
  add constraint runtime_settings_capacity_queue_ck check (
    capacity_queue_retry_seconds between 1 and 4294967295
    and capacity_queue_timeout_seconds between capacity_queue_retry_seconds and 4294967295
  );
