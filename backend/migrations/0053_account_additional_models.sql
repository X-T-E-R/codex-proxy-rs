-- 每账号自助登记的上游模型 ID，独立于仅用于收窄的模型政策
alter table provider_accounts
    add column additional_models_json jsonb not null default '[]'::jsonb;
