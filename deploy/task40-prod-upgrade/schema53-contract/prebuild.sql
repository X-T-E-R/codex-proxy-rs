\set ON_ERROR_STOP on
\set VERBOSITY verbose
\pset tuples_only on
\pset format unaligned
set search_path=public,pg_catalog;
set statement_timeout='5s';
set lock_timeout='500ms';
set application_name='cpr:schema53:prebuild';
do $$ begin
 if current_database()<>'codex_proxy' then raise exception using errcode='55000',message='CPR_SCHEMA53_PRECONDITION: wrong database'; end if;
 if not pg_try_advisory_lock(2426029098782969884) then raise exception using errcode='55P03',message='CPR_SCHEMA53_BUSY: SQLx migration lock is occupied'; end if;
end $$;
create temp table cpr_schema53_observed as select snapshot::jsonb from (with expected(version,checksum) as (values (1,'66daf12a89a1ec11a81110c1ce6289f110bf65beaa432f2668a2bddc6fd246933138dba2d70a60a3d29c6aa6b2e631c1'),
(2,'9c29e6c73340926f312b58af3b58b5be43b021ccc77797b4ebbf016cac6da802948d99f8e4a324cd1113184698e7ae8a'),
(3,'8a0ed3ad9db3132dfefe7bc60dc78558c6e1fc687575ec8f21c0a0e27c5aa0866fd1fbdaaa66a6ad82be0816ae41efc8'),
(4,'5379ecf2efdcc4c68c48a083284f38cef3be3e27cba46bf86d919649ab1622b4f86e4bbdae031758da1db56b2dd73860'),
(5,'a0691a7cb853a9e03abc94b0e3564384dedecfe8e6b68deaba4d059a7a9863764b28daa6dadacf24b53775c8cdeaa9b6'),
(6,'6d08edfc0b1ea7c2a13afa5ac318fb89f3dea6a91608918c27f1c86c7b3b1a515a43e43cbfd4cd09a1b74f86d9172581'),
(7,'6e1dd276f7f462fcd6f33299059b479e2f7a03b5d1f6abac78526c63fcc3546c61191cba62f91684e1450f11a6a6f2d7'),
(8,'85d41a20952cc776730ff3bd54f344e093895150b13b1fd3311694bd54e097bfb53197a4d248e576635c9eb380f5d68c'),
(9,'3639a56c6b43534e820814d14a985fda593241e341c4efb755c5eec3af233287a6542a90e3de907969a5f92e63d658fd'),
(10,'1eb2db277349cd1fea31adea40f26cd06c14e19061609ee29461df8aeb6b299e0af230d6b00eda434aed6b6227c38be3'),
(11,'7fffc53921126a187f4ced497916708aaa9c36e50135fb834a2a6655d4acc3c769b8cf55f65d4148e7c2b75d4b474af1'),
(12,'2f4243ffa1db5b3f9b18d1e12c5b84a18ef339ce798aa2ad8ba628f413424578a723c3598577b95ebadb8970fcdec799'),
(13,'0187b793d941d3ca14b646eeacfacb8efaf0c8a090d15de5d4807532d3c9416ce8f62f0aa1ba4822a417c19a196bf999'),
(14,'49d08f8e9f176f982bbaebee48e99d0e986a13425e270005c403159f2a91c4b70fa4ada5a9c5fa52dd31e4dcdcff825c'),
(15,'31592a8a58a8c51b15fc437e692e2c9cd2f73ccaa7fa29a0420bcb4f528461e10b419bc7898e97bcc595953d6ee05816'),
(16,'54ef952e48d3b077a024f381fa125772ca9ad143bd13073a28ccd2f672d1d68c0aa999cade3f6cc8d975ea2921142eb4'),
(17,'2a5fb4d567f7e960bcd03ce453019bb08509e9443a6ea81655de76ced5663704c59bcf5485dad3c15ab29cf602a2b781'),
(18,'c2359f68b0dcef276594a083f8e775a0c9a6c1ee940e830ba4ed9c82c05e8c18375059ae0cf1053e8c7dff13d904bda9'),
(19,'057f756f06cd28dc2d93f2c8b929c53d167dfa79805b14288551e5bb9023725f96b7cb4e1ee6baa82f79afae815b788e'),
(20,'1318527338be047862132d597ea2cb4e241a4de71eb408325635a33f7f8bd1a547b7acfef1fd896edbf239bf890449af'),
(21,'c4833587df037fbd3475b14431c6159404fe315953c7c0bdc35da8d359922259417e3a998454327ea079f399e19c9a9f'),
(22,'1bb51e09f04ddecf09072359e162e0cc464b966573aa263c550e1b3f455e4d3c1123d75d5804bdfc24d6d39914a7a103'),
(23,'975ccd8651930dc68ab1880023700358b38788d6a6ff7e47c0ee655e043bd6b39ada118ae11b8df15ec1bfbb501ac8f7'),
(24,'65239a803454864d6c3bc9cd12ad8a144119a1d3e66701c91a5f39a3b310060b830cf6d7eebc2882db12a72d1c05762e'),
(25,'53f2e8ed90475d91e06f47f42b7c0b255758d6d87ea97eb7d9922ca947f132249951ec0d329c4a2967111e275200e2e2'),
(26,'56c34c788d3ce23da2db8f2f8075806d7412c3d5c1f575e55a926c2570b918762cbe3a22487a7b0aadca9093dc3421ad'),
(27,'80700f493b474ea4680dcd0c1743b25b5d26bc88ef66ecbb9fdc73e4fa15d1bcf9ba576c77e1660437530cfc489bc94a'),
(28,'3237650cf0b58bd3fd3ae4d406ff669e392c24480b04ef890a39e9d1027952cb46d128606487adf43f8c21d32a88351b'),
(29,'51b9b0f1a0a393bbcb85ed2247356be4c3fb3156abc60cbb6fd67bac31bbd329b027cb49117467d12aa3871d368e41e6'),
(30,'6cb43e8906126bef733dc30049560d070b3516bf45aaddb0db5301351f73c0b75091cb979c028482d5880c6e24b6eb87'),
(31,'2347b6e9ac02cf28915f4726db3c23fadc91de72ba2ef55adaa88f2ec8a99c33bba2d3f2e0b0740ac1c51719b8c314dd'),
(32,'581c45781ff6da788ec3c2e253cbf3d712242323d95b42d88ade4a42b030b1fa92b895530e86b110656b893a0fca337e'),
(33,'ef9237262fa8af0ceba54d25a0cd40eb48fb9a2b74176c0c61d5c7b58a0fbfb3735ffc18509dcfce045f7595383eb1ac'),
(34,'fcd1405df2f2e5178ad363a9bf0e9b249ee418af7c29f8c20a8992f52eb15895f7b025036bf73510aadb9ba433c5af2e'),
(35,'1d564b11efda148812d6582b50e7c34722f28fe9e2ae8ad40ba5051e246f7e63344fdb40aa5043556abddcc4a44036ea'),
(36,'0e674b705b653e49384d51fb8e998696cbac58e8cc6e10bd5c86df970c2bfcc4e9eb0be45cd86502915ef46520f4005c'),
(37,'0fbcde03f0607aa735e0273444d443bac8dc6584ad73a21602635c2460a66a2be37721d3f79b69c1282cef5720797271'),
(38,'cdbd299f8d4732617b461b9e79b63a9564d06b4d2e5ad8e27b9759fb1c78260356fedae961955f339e54106233bec714'),
(39,'978f1125acb2356d1712dd5234fca33706f688f68b5d34810dc742c3df418f24f0e2444344d4c7699886cb22dec888ed'),
(40,'5828744bde0ab4757833628505aed0ee32429c297adc8e04dc781ca01647831703dfbf7da559c8fe8cb02ed2e58fa110'),
(41,'b71ec7935d77563aea52c65f4d0ee502adf9576324ccf9c7bb71e2a62355021e7a22511fbf1b6de16a73d533f9ecdec9'),
(42,'27daf72bd652f920bb9aabc4623c4ba4fe9f13f240ad440cf887cf85e66d6e1815a999ffb9df6085fdbe9414a0c493dc'),
(43,'c7444ad4ae60763556ed0342a3db77dd98c24c516a3ff2daad953c1b3a11d840144cbc72a6ac5d0c4aaeef660c467b96'),
(44,'a7919db9ba8c5e9632d94de590119f8c28abbf5949c837b868c72ee5137c9aa0cdfc978556cac0dc90fb2ef67db75193'),
(45,'7fad5059764a198c4fdd13a5b9121a22c2c7e01a77e93ac21118ac70a59b528920e3a3989659a788007054ae1afe110a'),
(46,'f0465575780a0df38fe41ace2b2fd879a1b004c08e2ad2ecc8bbf4589e65171adcad3bcf1fc1fb4661390ac302b09504'),
(47,'433f19a2d36f03cc5cac99af8f0eccb8bfe1950675258ee54d6cfc41026c8a423a89426f10521abdbfec7bfa1db37a11'),
(48,'94a16566faecb7cd8d30571f79d85a4c8100c5e0a5e78d2270f53565dfa68dee5f054d3ed2bb3d48eed3b78e4534890f'),
(49,'0170bd3361ce9ef50ec4a8ad9060f41e44b878ffcedaabffd2239f865b3f9a4940d4f73b4a7d90a8bc5b39210c4a94e7'),
(50,'165d53fa049638b0ffa1481cf19c8d7413c879de2588cbbd5d341e660434c57022adc2d2092d1bdc83594bbf57b86a6e'),
(51,'9c4fa946cc62abfd9411516859fb88e4084d0969276cf5c46180d83c818d30a16a1d8b0fa9952b073495b6b41d8d6c77'),
(52,'74569fc3c8fe4bb297e2f20fcde3d87fa32c7634ba4f681dcaa20cf3b046794a17eb16ef2249a1007c5c98fb25f2f527'),
(53,'d6eaa47ee3e93f871c3c74bb388d91a73ae25d1f9bbde4c4208bd6f32f5455de24c13ab630bb03a3e3ef0097b866c1d0')),
ledger as (
 select case
 when (select count(*) from _sqlx_migrations)=52
  and not exists(select 1 from expected e full join _sqlx_migrations a on a.version=e.version
   where coalesce(e.version,a.version)<=52 and (e.version is null or a.version is null or not a.success or encode(a.checksum,'hex')<>e.checksum))
  and not exists(select 1 from _sqlx_migrations where version>52) then 'schema52'
 when (select count(*) from _sqlx_migrations)=53
  and not exists(select 1 from expected e full join _sqlx_migrations a on a.version=e.version
   where e.version is null or a.version is null or not a.success or encode(a.checksum,'hex')<>e.checksum) then 'schema53'
 else 'mismatch' end as state
),
index_status as (
 select case when to_regclass('public.model_requests_successful_trace_pending_idx') is null then 'missing'
 when exists(select 1
        from pg_index i
        join pg_class c on c.oid = i.indexrelid
        join pg_am am on am.oid = c.relam
        join pg_attribute a on a.attrelid = i.indrelid and a.attname = 'completed_at'
        join pg_opclass op on op.oid = i.indclass[0]
        join pg_namespace op_namespace on op_namespace.oid = op.opcnamespace
        where i.indexrelid = to_regclass(format('%I.model_requests_successful_trace_pending_idx', current_schema()))
          and i.indrelid = 'model_requests'::regclass
          and i.indisvalid and i.indisready and i.indislive
          and not i.indisunique and not i.indisprimary and not i.indisexclusion
          and am.amname = 'btree'
          and i.indnkeyatts = 1 and i.indnatts = 1
          and i.indkey[0] = a.attnum and i.indoption[0] = 0
          and op_namespace.nspname = 'pg_catalog' and op.opcname = 'timestamptz_ops'
          and i.indexprs is null
          and pg_get_expr(i.indpred, i.indrelid) = '((outcome = ''succeeded''::text) AND (diagnostic_trace_json IS NOT NULL))') then 'valid'
 when exists(select 1
        from pg_index i
        join pg_class c on c.oid = i.indexrelid
        join pg_am am on am.oid = c.relam
        join pg_attribute a on a.attrelid = i.indrelid and a.attname = 'completed_at'
        join pg_opclass op on op.oid = i.indclass[0]
        join pg_namespace op_namespace on op_namespace.oid = op.opcnamespace
        where i.indexrelid = to_regclass(format('%I.model_requests_successful_trace_pending_idx', current_schema()))
          and i.indrelid = 'model_requests'::regclass
          and (not i.indisvalid or not i.indisready) and i.indislive
          and not i.indisunique and not i.indisprimary and not i.indisexclusion
          and am.amname = 'btree'
          and i.indnkeyatts = 1 and i.indnatts = 1
          and i.indkey[0] = a.attnum and i.indoption[0] = 0
          and op_namespace.nspname = 'pg_catalog' and op.opcname = 'timestamptz_ops'
          and i.indexprs is null
          and pg_get_expr(i.indpred, i.indrelid) = '((outcome = ''succeeded''::text) AND (diagnostic_trace_json IS NOT NULL))') then 'invalid'
 when exists(select 1
        from pg_index i
        join pg_class c on c.oid = i.indexrelid
        join pg_am am on am.oid = c.relam
        join pg_attribute a on a.attrelid = i.indrelid and a.attname = 'completed_at'
        join pg_opclass op on op.oid = i.indclass[0]
        join pg_namespace op_namespace on op_namespace.oid = op.opcnamespace
        where i.indexrelid = to_regclass(format('%I.model_requests_successful_trace_pending_idx', current_schema()))
          and i.indrelid = 'model_requests'::regclass
          and not i.indisvalid and not i.indisready and not i.indislive
          and not i.indisunique and not i.indisprimary and not i.indisexclusion
          and am.amname = 'btree'
          and i.indnkeyatts = 1 and i.indnatts = 1
          and i.indkey[0] = a.attnum and i.indoption[0] = 0
          and op_namespace.nspname = 'pg_catalog' and op.opcname = 'timestamptz_ops'
          and i.indexprs is null
          and pg_get_expr(i.indpred, i.indrelid) = '((outcome = ''succeeded''::text) AND (diagnostic_trace_json IS NOT NULL))') then 'drop_incomplete'
 else 'unexpected' end as state
)
select json_build_object('contract','cpr-successful-trace-index/3','schema_state',ledger.state,
 'schema_version',(select max(version) from _sqlx_migrations),'index_state',index_status.state,
 'active_operations',(select coalesce(json_agg(json_build_object('pid',pid,'application_name',application_name,'state',state,'wait_event_type',wait_event_type,'wait_event',wait_event)),'[]'::json)
 from pg_stat_activity where datname=current_database() and pid<>pg_backend_pid()
  and (application_name like 'cpr:schema53:%' or application_name='codex-proxy-rs:migration')))
from ledger cross join index_status) as q(snapshot);
do $$ begin if not (select snapshot->>'schema_state'='schema52' and snapshot->>'index_state' in ('missing','valid') from cpr_schema53_observed) then raise exception using errcode='55000',message='CPR_SCHEMA53_PRECONDITION: requires exact schema52 and missing or valid index'; end if; end $$;
select command from (values
 (1, 'set statement_timeout=''120s'''),
 (2, 'create index concurrently model_requests_successful_trace_pending_idx on model_requests using btree (completed_at) where outcome = ''succeeded'' and diagnostic_trace_json is not null'),
 (3, 'set statement_timeout=''5s''')
) as ddl(ordinal,command) where (select snapshot->>'index_state' from cpr_schema53_observed)='missing' order by ordinal
\gexec
with expected(version,checksum) as (values (1,'66daf12a89a1ec11a81110c1ce6289f110bf65beaa432f2668a2bddc6fd246933138dba2d70a60a3d29c6aa6b2e631c1'),
(2,'9c29e6c73340926f312b58af3b58b5be43b021ccc77797b4ebbf016cac6da802948d99f8e4a324cd1113184698e7ae8a'),
(3,'8a0ed3ad9db3132dfefe7bc60dc78558c6e1fc687575ec8f21c0a0e27c5aa0866fd1fbdaaa66a6ad82be0816ae41efc8'),
(4,'5379ecf2efdcc4c68c48a083284f38cef3be3e27cba46bf86d919649ab1622b4f86e4bbdae031758da1db56b2dd73860'),
(5,'a0691a7cb853a9e03abc94b0e3564384dedecfe8e6b68deaba4d059a7a9863764b28daa6dadacf24b53775c8cdeaa9b6'),
(6,'6d08edfc0b1ea7c2a13afa5ac318fb89f3dea6a91608918c27f1c86c7b3b1a515a43e43cbfd4cd09a1b74f86d9172581'),
(7,'6e1dd276f7f462fcd6f33299059b479e2f7a03b5d1f6abac78526c63fcc3546c61191cba62f91684e1450f11a6a6f2d7'),
(8,'85d41a20952cc776730ff3bd54f344e093895150b13b1fd3311694bd54e097bfb53197a4d248e576635c9eb380f5d68c'),
(9,'3639a56c6b43534e820814d14a985fda593241e341c4efb755c5eec3af233287a6542a90e3de907969a5f92e63d658fd'),
(10,'1eb2db277349cd1fea31adea40f26cd06c14e19061609ee29461df8aeb6b299e0af230d6b00eda434aed6b6227c38be3'),
(11,'7fffc53921126a187f4ced497916708aaa9c36e50135fb834a2a6655d4acc3c769b8cf55f65d4148e7c2b75d4b474af1'),
(12,'2f4243ffa1db5b3f9b18d1e12c5b84a18ef339ce798aa2ad8ba628f413424578a723c3598577b95ebadb8970fcdec799'),
(13,'0187b793d941d3ca14b646eeacfacb8efaf0c8a090d15de5d4807532d3c9416ce8f62f0aa1ba4822a417c19a196bf999'),
(14,'49d08f8e9f176f982bbaebee48e99d0e986a13425e270005c403159f2a91c4b70fa4ada5a9c5fa52dd31e4dcdcff825c'),
(15,'31592a8a58a8c51b15fc437e692e2c9cd2f73ccaa7fa29a0420bcb4f528461e10b419bc7898e97bcc595953d6ee05816'),
(16,'54ef952e48d3b077a024f381fa125772ca9ad143bd13073a28ccd2f672d1d68c0aa999cade3f6cc8d975ea2921142eb4'),
(17,'2a5fb4d567f7e960bcd03ce453019bb08509e9443a6ea81655de76ced5663704c59bcf5485dad3c15ab29cf602a2b781'),
(18,'c2359f68b0dcef276594a083f8e775a0c9a6c1ee940e830ba4ed9c82c05e8c18375059ae0cf1053e8c7dff13d904bda9'),
(19,'057f756f06cd28dc2d93f2c8b929c53d167dfa79805b14288551e5bb9023725f96b7cb4e1ee6baa82f79afae815b788e'),
(20,'1318527338be047862132d597ea2cb4e241a4de71eb408325635a33f7f8bd1a547b7acfef1fd896edbf239bf890449af'),
(21,'c4833587df037fbd3475b14431c6159404fe315953c7c0bdc35da8d359922259417e3a998454327ea079f399e19c9a9f'),
(22,'1bb51e09f04ddecf09072359e162e0cc464b966573aa263c550e1b3f455e4d3c1123d75d5804bdfc24d6d39914a7a103'),
(23,'975ccd8651930dc68ab1880023700358b38788d6a6ff7e47c0ee655e043bd6b39ada118ae11b8df15ec1bfbb501ac8f7'),
(24,'65239a803454864d6c3bc9cd12ad8a144119a1d3e66701c91a5f39a3b310060b830cf6d7eebc2882db12a72d1c05762e'),
(25,'53f2e8ed90475d91e06f47f42b7c0b255758d6d87ea97eb7d9922ca947f132249951ec0d329c4a2967111e275200e2e2'),
(26,'56c34c788d3ce23da2db8f2f8075806d7412c3d5c1f575e55a926c2570b918762cbe3a22487a7b0aadca9093dc3421ad'),
(27,'80700f493b474ea4680dcd0c1743b25b5d26bc88ef66ecbb9fdc73e4fa15d1bcf9ba576c77e1660437530cfc489bc94a'),
(28,'3237650cf0b58bd3fd3ae4d406ff669e392c24480b04ef890a39e9d1027952cb46d128606487adf43f8c21d32a88351b'),
(29,'51b9b0f1a0a393bbcb85ed2247356be4c3fb3156abc60cbb6fd67bac31bbd329b027cb49117467d12aa3871d368e41e6'),
(30,'6cb43e8906126bef733dc30049560d070b3516bf45aaddb0db5301351f73c0b75091cb979c028482d5880c6e24b6eb87'),
(31,'2347b6e9ac02cf28915f4726db3c23fadc91de72ba2ef55adaa88f2ec8a99c33bba2d3f2e0b0740ac1c51719b8c314dd'),
(32,'581c45781ff6da788ec3c2e253cbf3d712242323d95b42d88ade4a42b030b1fa92b895530e86b110656b893a0fca337e'),
(33,'ef9237262fa8af0ceba54d25a0cd40eb48fb9a2b74176c0c61d5c7b58a0fbfb3735ffc18509dcfce045f7595383eb1ac'),
(34,'fcd1405df2f2e5178ad363a9bf0e9b249ee418af7c29f8c20a8992f52eb15895f7b025036bf73510aadb9ba433c5af2e'),
(35,'1d564b11efda148812d6582b50e7c34722f28fe9e2ae8ad40ba5051e246f7e63344fdb40aa5043556abddcc4a44036ea'),
(36,'0e674b705b653e49384d51fb8e998696cbac58e8cc6e10bd5c86df970c2bfcc4e9eb0be45cd86502915ef46520f4005c'),
(37,'0fbcde03f0607aa735e0273444d443bac8dc6584ad73a21602635c2460a66a2be37721d3f79b69c1282cef5720797271'),
(38,'cdbd299f8d4732617b461b9e79b63a9564d06b4d2e5ad8e27b9759fb1c78260356fedae961955f339e54106233bec714'),
(39,'978f1125acb2356d1712dd5234fca33706f688f68b5d34810dc742c3df418f24f0e2444344d4c7699886cb22dec888ed'),
(40,'5828744bde0ab4757833628505aed0ee32429c297adc8e04dc781ca01647831703dfbf7da559c8fe8cb02ed2e58fa110'),
(41,'b71ec7935d77563aea52c65f4d0ee502adf9576324ccf9c7bb71e2a62355021e7a22511fbf1b6de16a73d533f9ecdec9'),
(42,'27daf72bd652f920bb9aabc4623c4ba4fe9f13f240ad440cf887cf85e66d6e1815a999ffb9df6085fdbe9414a0c493dc'),
(43,'c7444ad4ae60763556ed0342a3db77dd98c24c516a3ff2daad953c1b3a11d840144cbc72a6ac5d0c4aaeef660c467b96'),
(44,'a7919db9ba8c5e9632d94de590119f8c28abbf5949c837b868c72ee5137c9aa0cdfc978556cac0dc90fb2ef67db75193'),
(45,'7fad5059764a198c4fdd13a5b9121a22c2c7e01a77e93ac21118ac70a59b528920e3a3989659a788007054ae1afe110a'),
(46,'f0465575780a0df38fe41ace2b2fd879a1b004c08e2ad2ecc8bbf4589e65171adcad3bcf1fc1fb4661390ac302b09504'),
(47,'433f19a2d36f03cc5cac99af8f0eccb8bfe1950675258ee54d6cfc41026c8a423a89426f10521abdbfec7bfa1db37a11'),
(48,'94a16566faecb7cd8d30571f79d85a4c8100c5e0a5e78d2270f53565dfa68dee5f054d3ed2bb3d48eed3b78e4534890f'),
(49,'0170bd3361ce9ef50ec4a8ad9060f41e44b878ffcedaabffd2239f865b3f9a4940d4f73b4a7d90a8bc5b39210c4a94e7'),
(50,'165d53fa049638b0ffa1481cf19c8d7413c879de2588cbbd5d341e660434c57022adc2d2092d1bdc83594bbf57b86a6e'),
(51,'9c4fa946cc62abfd9411516859fb88e4084d0969276cf5c46180d83c818d30a16a1d8b0fa9952b073495b6b41d8d6c77'),
(52,'74569fc3c8fe4bb297e2f20fcde3d87fa32c7634ba4f681dcaa20cf3b046794a17eb16ef2249a1007c5c98fb25f2f527'),
(53,'d6eaa47ee3e93f871c3c74bb388d91a73ae25d1f9bbde4c4208bd6f32f5455de24c13ab630bb03a3e3ef0097b866c1d0')),
ledger as (
 select case
 when (select count(*) from _sqlx_migrations)=52
  and not exists(select 1 from expected e full join _sqlx_migrations a on a.version=e.version
   where coalesce(e.version,a.version)<=52 and (e.version is null or a.version is null or not a.success or encode(a.checksum,'hex')<>e.checksum))
  and not exists(select 1 from _sqlx_migrations where version>52) then 'schema52'
 when (select count(*) from _sqlx_migrations)=53
  and not exists(select 1 from expected e full join _sqlx_migrations a on a.version=e.version
   where e.version is null or a.version is null or not a.success or encode(a.checksum,'hex')<>e.checksum) then 'schema53'
 else 'mismatch' end as state
),
index_status as (
 select case when to_regclass('public.model_requests_successful_trace_pending_idx') is null then 'missing'
 when exists(select 1
        from pg_index i
        join pg_class c on c.oid = i.indexrelid
        join pg_am am on am.oid = c.relam
        join pg_attribute a on a.attrelid = i.indrelid and a.attname = 'completed_at'
        join pg_opclass op on op.oid = i.indclass[0]
        join pg_namespace op_namespace on op_namespace.oid = op.opcnamespace
        where i.indexrelid = to_regclass(format('%I.model_requests_successful_trace_pending_idx', current_schema()))
          and i.indrelid = 'model_requests'::regclass
          and i.indisvalid and i.indisready and i.indislive
          and not i.indisunique and not i.indisprimary and not i.indisexclusion
          and am.amname = 'btree'
          and i.indnkeyatts = 1 and i.indnatts = 1
          and i.indkey[0] = a.attnum and i.indoption[0] = 0
          and op_namespace.nspname = 'pg_catalog' and op.opcname = 'timestamptz_ops'
          and i.indexprs is null
          and pg_get_expr(i.indpred, i.indrelid) = '((outcome = ''succeeded''::text) AND (diagnostic_trace_json IS NOT NULL))') then 'valid'
 when exists(select 1
        from pg_index i
        join pg_class c on c.oid = i.indexrelid
        join pg_am am on am.oid = c.relam
        join pg_attribute a on a.attrelid = i.indrelid and a.attname = 'completed_at'
        join pg_opclass op on op.oid = i.indclass[0]
        join pg_namespace op_namespace on op_namespace.oid = op.opcnamespace
        where i.indexrelid = to_regclass(format('%I.model_requests_successful_trace_pending_idx', current_schema()))
          and i.indrelid = 'model_requests'::regclass
          and (not i.indisvalid or not i.indisready) and i.indislive
          and not i.indisunique and not i.indisprimary and not i.indisexclusion
          and am.amname = 'btree'
          and i.indnkeyatts = 1 and i.indnatts = 1
          and i.indkey[0] = a.attnum and i.indoption[0] = 0
          and op_namespace.nspname = 'pg_catalog' and op.opcname = 'timestamptz_ops'
          and i.indexprs is null
          and pg_get_expr(i.indpred, i.indrelid) = '((outcome = ''succeeded''::text) AND (diagnostic_trace_json IS NOT NULL))') then 'invalid'
 when exists(select 1
        from pg_index i
        join pg_class c on c.oid = i.indexrelid
        join pg_am am on am.oid = c.relam
        join pg_attribute a on a.attrelid = i.indrelid and a.attname = 'completed_at'
        join pg_opclass op on op.oid = i.indclass[0]
        join pg_namespace op_namespace on op_namespace.oid = op.opcnamespace
        where i.indexrelid = to_regclass(format('%I.model_requests_successful_trace_pending_idx', current_schema()))
          and i.indrelid = 'model_requests'::regclass
          and not i.indisvalid and not i.indisready and not i.indislive
          and not i.indisunique and not i.indisprimary and not i.indisexclusion
          and am.amname = 'btree'
          and i.indnkeyatts = 1 and i.indnatts = 1
          and i.indkey[0] = a.attnum and i.indoption[0] = 0
          and op_namespace.nspname = 'pg_catalog' and op.opcname = 'timestamptz_ops'
          and i.indexprs is null
          and pg_get_expr(i.indpred, i.indrelid) = '((outcome = ''succeeded''::text) AND (diagnostic_trace_json IS NOT NULL))') then 'drop_incomplete'
 else 'unexpected' end as state
)
select json_build_object('contract','cpr-successful-trace-index/3','schema_state',ledger.state,
 'schema_version',(select max(version) from _sqlx_migrations),'index_state',index_status.state,
 'active_operations',(select coalesce(json_agg(json_build_object('pid',pid,'application_name',application_name,'state',state,'wait_event_type',wait_event_type,'wait_event',wait_event)),'[]'::json)
 from pg_stat_activity where datname=current_database() and pid<>pg_backend_pid()
  and (application_name like 'cpr:schema53:%' or application_name='codex-proxy-rs:migration')))
from ledger cross join index_status;
