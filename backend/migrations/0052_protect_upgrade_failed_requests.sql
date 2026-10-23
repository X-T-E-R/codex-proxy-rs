-- 首轮升级不让统一保留期删除已有失败，后续释放保护须独立决定
create table upgrade_retention_protected_requests (
    request_id text primary key references model_requests(id) on delete cascade
);
insert into upgrade_retention_protected_requests(request_id)
select id from model_requests where outcome not in ('running','succeeded');
