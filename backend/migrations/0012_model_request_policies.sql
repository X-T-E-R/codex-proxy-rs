-- 模型级请求策略：按客户端请求模型精确匹配，冻结到路由计划后由 Provider 改写请求。
-- 仅约束 JSON 形状；条目数（≤512）与模型名（≤256 UTF-8 字节）上限由 API 与 store 校验保证，
-- 数据库不再重复字节上限，避免转义后文本长度与应用层校验口径不一致。
alter table runtime_settings
    add column model_policies_json jsonb not null default '{}'::jsonb,
    add constraint runtime_settings_model_policies_ck check (
        jsonb_typeof(model_policies_json) = 'object'
    );
