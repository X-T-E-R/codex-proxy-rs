//! 请求列表的精确总数、双向分页与筛选隔离

use chrono::{TimeDelta, Utc};
use gateway_store::postgres::{
    ObservabilityPageSize, ObservabilityRange, OpsErrorFilter, OpsErrorQuery, UsageRecordFilter,
    UsageRecordQuery,
};

use super::{TestDatabase, analytics::seed, observability_repository};

#[tokio::test]
async fn projected_page_counts_correct_pending_moves_deletions_and_model_or() {
    use gateway_admin::model::observability::UsageRecordStatus;

    let Some(database) = TestDatabase::create("usage_projected_pages").await else {
        return;
    };
    let start = chrono::DateTime::parse_from_rfc3339("2026-09-01T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    for index in 0..40 {
        seed(
            &database.pool,
            &format!("projected_{index:02}"),
            start + TimeDelta::minutes(index),
            &[],
        )
        .await;
    }
    super::drain_analytics(&database.pool).await;
    let repository = observability_repository(&database.pool);
    for stage in 0..3 {
        if stage == 1 {
            sqlx::query("update model_requests set started_at=started_at+interval '30 minutes',recovery_lease_expires_at=recovery_lease_expires_at+interval '30 minutes',completed_at=completed_at+interval '30 minutes',requested_model_id='model_b',upstream_model_id='model_b' where id='projected_06'")
                .execute(&database.pool).await.unwrap();
            sqlx::query("delete from model_requests where id='projected_17'")
                .execute(&database.pool)
                .await
                .unwrap();
            sqlx::query("update model_requests set outcome='failed',upstream_model_id='model_b',recovery_request_id='projected_22',recovered_at=$1,recovery_attempt_count=1,request_observation_json=jsonb_set(request_observation_json,'{recovery}',jsonb_build_object('retryDelayMs',0,'totalLatencyMs',1)) where id='projected_21'")
                .bind(start + TimeDelta::minutes(22)).execute(&database.pool).await.unwrap();
            sqlx::query("update model_requests set outcome=case id when 'projected_23' then 'running' when 'projected_24' then 'cancelled' else 'incomplete' end,completed_at=case when id='projected_23' then null else completed_at end,requested_model_id=case when id in ('projected_24','projected_25') then null else requested_model_id end,upstream_model_id=case when id='projected_25' then 'model_b' else upstream_model_id end where id in ('projected_23','projected_24','projected_25')")
                .execute(&database.pool).await.unwrap();
            seed(
                &database.pool,
                "projected_new",
                start + TimeDelta::seconds(30),
                &[],
            )
            .await;
        } else if stage == 2 {
            super::drain_analytics(&database.pool).await;
        }
        for edge in [0, 31] {
            let range = ObservabilityRange::new(
                start + TimeDelta::seconds(edge),
                start + TimeDelta::hours(1) - TimeDelta::seconds(edge),
            )
            .unwrap();
            for model in [None, Some("model_a"), Some("model_b")] {
                for status in [UsageRecordStatus::Success, UsageRecordStatus::All] {
                    // 夹具中成功记录均已交付；真值只读源版本，不依赖投影或分页辅助函数
                    let expected:Vec<String> = sqlx::query_scalar("select id from model_requests where started_at >= $1 and started_at < $2 and ($4::bool or outcome='succeeded') and ($3::text is null or requested_model_id=$3 or upstream_model_id=$3) order by started_at desc,id desc")
                        .bind(range.start).bind(range.end).bind(model).bind(status == UsageRecordStatus::All).fetch_all(&database.pool).await.unwrap();
                    for current_page in [1, 3, 6, 8] {
                        let page = repository
                            .list_usage_records(UsageRecordQuery {
                                status,
                                range,
                                filter: UsageRecordFilter {
                                    client_api_key_ref: Some("key_analytics".to_owned()),
                                    model: model.map(str::to_owned),
                                    ..Default::default()
                                },
                                current_page,
                                page_size: ObservabilityPageSize::new(7).unwrap(),
                            })
                            .await
                            .unwrap();
                        assert_eq!(
                            page.total,
                            expected.len() as u64,
                            "stage={stage}, edge={edge}, model={model:?}, status={status:?}"
                        );
                        assert_eq!(
                            page.items.iter().map(|row| &row.id).collect::<Vec<_>>(),
                            expected
                                .iter()
                                .skip((current_page as usize - 1) * 7)
                                .take(7)
                                .collect::<Vec<_>>()
                        );
                    }
                }
            }
        }
    }
    database.close().await;
}

#[tokio::test]
async fn usage_pages_preserve_order_and_exact_totals_from_both_ends() {
    let Some(database) = TestDatabase::create("usage_bidirectional_pages").await else {
        return;
    };
    let start = Utc::now() - TimeDelta::hours(1);
    seed(&database.pool, "page_00", start, &[]).await;
    sqlx::query("insert into model_requests select cloned.* from model_requests original cross join generate_series(1,22) n cross join lateral jsonb_populate_record(null::model_requests,to_jsonb(original)||jsonb_build_object('id','page_'||lpad(n::text,2,'0'),'client_api_key_ref',case when n%2=0 then 'key_analytics' else 'other_key' end)) cloned where original.id='page_00'")
        .execute(&database.pool).await.unwrap();
    let range =
        ObservabilityRange::new(start - TimeDelta::seconds(1), start + TimeDelta::seconds(1))
            .unwrap();
    let repository = observability_repository(&database.pool);
    for key in [
        None,
        Some("key_analytics"),
        Some("other_key"),
        Some("absent_key"),
    ] {
        let expected = (0..23)
            .rev()
            .filter(|n| match key {
                None => true,
                Some("key_analytics") => n % 2 == 0,
                Some("other_key") => n % 2 != 0,
                Some(_) => false,
            })
            .map(|n| format!("page_{n:02}"))
            .collect::<Vec<_>>();
        for page in 1..=5 {
            let result = repository
                .list_usage_records(UsageRecordQuery {
                    status: Default::default(),
                    range,
                    filter: UsageRecordFilter {
                        client_api_key_ref: key.map(str::to_owned),
                        ..Default::default()
                    },
                    current_page: page,
                    page_size: ObservabilityPageSize::new(7).unwrap(),
                })
                .await
                .unwrap();
            let ids = result
                .items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>();
            let expected_page = expected
                .iter()
                .skip((page as usize - 1) * 7)
                .take(7)
                .map(String::as_str)
                .collect::<Vec<_>>();
            assert_eq!(
                result.total,
                expected.len() as u64,
                "key={key:?}, page={page}"
            );
            assert_eq!(ids, expected_page, "key={key:?}, page={page}");
        }
    }
    database.close().await;
}

#[tokio::test]
async fn usage_pages_cross_minutes_and_keep_exact_counts_after_source_changes() {
    let Some(database) = TestDatabase::create("usage_time_pages").await else {
        return;
    };
    let start = Utc::now() - TimeDelta::hours(1);
    for index in 0..29 {
        seed(
            &database.pool,
            &format!("cross_{index:02}"),
            start + TimeDelta::seconds((index / 3) * 100),
            &["group_page"],
        )
        .await;
    }
    let repository = observability_repository(&database.pool);
    let range = ObservabilityRange::new(start, start + TimeDelta::minutes(20)).unwrap();
    for removed in [None, Some("cross_16")] {
        if let Some(id) = removed {
            sqlx::query("delete from model_requests where id = $1")
                .bind(id)
                .execute(&database.pool)
                .await
                .unwrap();
        }
        let expected = (0..29)
            .rev()
            .map(|index| format!("cross_{index:02}"))
            .filter(|id| Some(id.as_str()) != removed)
            .collect::<Vec<_>>();
        for current_page in 1..=6 {
            let page = repository
                .list_usage_records(UsageRecordQuery {
                    status: Default::default(),
                    range,
                    filter: UsageRecordFilter {
                        client_api_key_ref: Some("key_analytics".to_owned()),
                        search: Some("model_".to_owned()),
                        ..Default::default()
                    },
                    current_page,
                    page_size: ObservabilityPageSize::new(7).unwrap(),
                })
                .await
                .unwrap();
            assert_eq!(page.total, expected.len() as u64);
            assert_eq!(
                page.items.iter().map(|item| &item.id).collect::<Vec<_>>(),
                expected
                    .iter()
                    .skip((current_page as usize - 1) * 7)
                    .take(7)
                    .collect::<Vec<_>>(),
                "page={current_page}, removed={removed:?}",
            );
        }
    }
    database.close().await;
}

#[tokio::test]
async fn ops_pages_merge_sources_across_minutes_with_stable_ties() {
    let Some(database) = TestDatabase::create("ops_time_pages").await else {
        return;
    };
    let start = Utc::now() - TimeDelta::hours(1);
    let mut expected = Vec::new();
    for index in 0..17 {
        let id = format!("error_{index:02}");
        let at = start + TimeDelta::seconds((index / 3) * 100);
        seed(&database.pool, &id, at, &[]).await;
        sqlx::query("update model_requests set error_kind = 'upstream_error' where id = $1")
            .bind(&id)
            .execute(&database.pool)
            .await
            .unwrap();
        sqlx::query(
            "insert into ops_events (id, model_request_id, attempt_index, level, component,
                 operation, failure_kind, message, created_at)
             values ($1, $1, 1, 'error', 'routing', 'responses', 'upstream_error', 'fixture', $2)",
        )
        .bind(&id)
        .bind(at + TimeDelta::minutes(1))
        .execute(&database.pool)
        .await
        .unwrap();
        for source in ["model_request", "ops_event"] {
            expected.push((at + TimeDelta::minutes(1), format!("{source}:{id}")));
        }
    }
    expected.sort_by(|a, b| b.cmp(a));
    let repository = observability_repository(&database.pool);
    let range = ObservabilityRange::new(start, start + TimeDelta::minutes(20)).unwrap();
    for current_page in 1..=7 {
        let page = repository
            .list_ops_errors(OpsErrorQuery {
                range,
                filter: OpsErrorFilter {
                    client_api_key_ref: Some("key_analytics".to_owned()),
                    search: Some("error_".to_owned()),
                    ..Default::default()
                },
                current_page,
                page_size: ObservabilityPageSize::new(7).unwrap(),
            })
            .await
            .unwrap();
        assert_eq!(page.total, expected.len() as u64);
        assert_eq!(
            page.items
                .iter()
                .map(|item| &item.stable_sort_id)
                .collect::<Vec<_>>(),
            expected
                .iter()
                .skip((current_page as usize - 1) * 7)
                .take(7)
                .map(|(_, id)| id)
                .collect::<Vec<_>>(),
            "page={current_page}",
        );
    }
    database.close().await;
}

#[tokio::test]
async fn all_status_counts_keep_filters_delivery_rules_and_time_boundaries() {
    use gateway_admin::model::observability::UsageRecordStatus;

    let Some(database) = TestDatabase::create("usage_all_counts").await else {
        return;
    };
    let start = chrono::DateTime::parse_from_rfc3339("2026-09-01T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    for (id, seconds, outcome) in [
        ("count_success", 0, "succeeded"),
        ("count_failed", 10, "failed"),
        ("count_running", 60, "running"),
        ("count_cancelled", 70, "cancelled"),
        ("count_incomplete", 120, "incomplete"),
        ("count_success_ws", 130, "succeeded"),
        ("count_prewarm", 140, "succeeded"),
        ("count_uncommitted", 145, "succeeded"),
        ("count_before", -1, "failed"),
        ("count_end", 180, "failed"),
    ] {
        seed(&database.pool, id, start + TimeDelta::seconds(seconds), &[]).await;
        sqlx::query("update model_requests set outcome=$2,provider_kind='openai',completed_at=case when $2='running' then null else completed_at end,input_tokens=case when $2='succeeded' then input_tokens else null end,output_tokens=case when $2='succeeded' then output_tokens else null end,total_tokens=case when $2='succeeded' then total_tokens else null end where id=$1")
            .bind(id).bind(outcome).execute(&database.pool).await.unwrap();
    }
    sqlx::query("update model_requests set recovery_request_id='count_success',recovered_at=$1,recovery_attempt_count=1,request_observation_json=jsonb_set(request_observation_json,'{recovery}',jsonb_build_object('retryDelayMs',1000,'totalLatencyMs',2000)) where id='count_failed'")
        .bind(start + TimeDelta::minutes(2)).execute(&database.pool).await.unwrap();
    sqlx::query("update model_requests set upstream_model_id='model_a' where id='count_failed'")
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("update model_requests set requested_model_id=null,upstream_model_id='model_b' where id='count_incomplete'")
        .execute(&database.pool).await.unwrap();
    sqlx::query("update model_requests set client_api_key_ref='key_other',requested_model_id='model_b' where id='count_running'")
        .execute(&database.pool).await.unwrap();
    sqlx::query("update model_requests set client_transport='websocket',client_status_code=null,requested_model_id='model_b' where id='count_success_ws'")
        .execute(&database.pool).await.unwrap();
    sqlx::query("update model_requests set request_kind='prewarm' where id='count_prewarm'")
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query(
        "update model_requests set downstream_committed_at=null where id='count_uncommitted'",
    )
    .execute(&database.pool)
    .await
    .unwrap();
    // 恢复错误仍属于列表；尚未投影的源记录也必须参与精确计数
    let repo = observability_repository(&database.pool);
    let range = ObservabilityRange::new(start, start + TimeDelta::minutes(3)).unwrap();
    let all = vec![
        "count_success_ws",
        "count_incomplete",
        "count_cancelled",
        "count_running",
        "count_failed",
        "count_success",
    ];
    let model_a = vec!["count_cancelled", "count_failed", "count_success"];
    for (filter, expected) in [
        (UsageRecordFilter::default(), all.clone()),
        (
            UsageRecordFilter {
                search: Some("model_".into()),
                ..Default::default()
            },
            all,
        ),
        (
            UsageRecordFilter {
                client_api_key_ref: Some("key_analytics".into()),
                ..Default::default()
            },
            vec![
                "count_success_ws",
                "count_incomplete",
                "count_cancelled",
                "count_failed",
                "count_success",
            ],
        ),
        (
            UsageRecordFilter {
                model: Some("model_a".into()),
                ..Default::default()
            },
            model_a.clone(),
        ),
        (
            UsageRecordFilter {
                provider_kind: Some("openai".into()),
                model: Some("model_a".into()),
                ..Default::default()
            },
            model_a.clone(),
        ),
        (
            UsageRecordFilter {
                provider_kind: Some("other".into()),
                model: Some("model_a".into()),
                ..Default::default()
            },
            vec![],
        ),
        (
            UsageRecordFilter {
                provider_kind: Some("openai".into()),
                model: Some("model_a".into()),
                outcome: Some("failed".into()),
                ..Default::default()
            },
            vec!["count_failed"],
        ),
        (
            UsageRecordFilter {
                search: Some("model_a".into()),
                ..Default::default()
            },
            model_a,
        ),
        (
            UsageRecordFilter {
                search: Some("no_such_model".into()),
                ..Default::default()
            },
            vec![],
        ),
        (
            UsageRecordFilter {
                model: Some("model_b".into()),
                ..Default::default()
            },
            vec!["count_success_ws", "count_incomplete", "count_running"],
        ),
        (
            UsageRecordFilter {
                provider_kind: Some("openai".into()),
                model: Some("model_b".into()),
                ..Default::default()
            },
            vec!["count_success_ws", "count_incomplete", "count_running"],
        ),
        (
            UsageRecordFilter {
                outcome: Some("failed".into()),
                ..Default::default()
            },
            vec!["count_failed"],
        ),
        (
            UsageRecordFilter {
                request_id: Some("count_failed".into()),
                ..Default::default()
            },
            vec!["count_failed"],
        ),
        (
            UsageRecordFilter {
                transport: Some("websocket".into()),
                ..Default::default()
            },
            vec!["count_success_ws"],
        ),
        (
            UsageRecordFilter {
                provider_kind: Some("other".into()),
                ..Default::default()
            },
            vec![],
        ),
    ] {
        for status in [UsageRecordStatus::All, UsageRecordStatus::Error] {
            let expected = expected
                .iter()
                .copied()
                .filter(|id| {
                    status == UsageRecordStatus::All
                        || ["count_failed", "count_incomplete"].contains(id)
                })
                .collect::<Vec<_>>();
            let mut actual = Vec::new();
            for current_page in 1..=4 {
                let page = repo
                    .list_usage_records(UsageRecordQuery {
                        status,
                        range,
                        filter: filter.clone(),
                        current_page,
                        page_size: ObservabilityPageSize::new(2).unwrap(),
                    })
                    .await
                    .unwrap();
                assert_eq!(
                    page.total,
                    expected.len() as u64,
                    "{status:?} {filter:?} page {current_page}"
                );
                actual.extend(page.items.into_iter().map(|item| item.id));
            }
            assert_eq!(actual, expected, "{status:?} {filter:?}");
        }
    }
    super::drain_analytics(&database.pool).await;
    // 聚合已经发布，但跨维筛选仍走源表；已恢复错误不能因汇总不包含它而漏计
    let page = repo
        .list_usage_records(UsageRecordQuery {
            status: UsageRecordStatus::All,
            range,
            filter: UsageRecordFilter {
                client_api_key_ref: Some("key_analytics".into()),
                transport: Some("http_sse".into()),
                model: Some("model_a".into()),
                ..Default::default()
            },
            current_page: 2,
            page_size: ObservabilityPageSize::new(2).unwrap(),
        })
        .await
        .unwrap();
    assert_eq!(page.total, 3);
    assert_eq!(page.items[0].id, "count_success");
    // 发布成功聚合后，供应方与模型仍共同限制源表非成功记录
    for (provider, total, expected) in [("openai", 3, vec!["count_success"]), ("other", 0, vec![])]
    {
        let page = repo
            .list_usage_records(UsageRecordQuery {
                status: UsageRecordStatus::All,
                range,
                filter: UsageRecordFilter {
                    provider_kind: Some(provider.into()),
                    model: Some("model_a".into()),
                    ..Default::default()
                },
                current_page: 2,
                page_size: ObservabilityPageSize::new(2).unwrap(),
            })
            .await
            .unwrap();
        assert_eq!(page.total, total, "{provider}");
        assert_eq!(
            page.items
                .into_iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            expected,
            "{provider}",
        );
    }
    database.close().await;
}

#[tokio::test]
async fn request_status_pages_share_one_request_timeline() {
    use gateway_admin::{
        model::{diagnostics::DiagnosticTarget, observability::UsageRecordStatus},
        ports::store::ObservabilityStore,
    };

    let Some(database) = TestDatabase::create("usage_status_pages").await else {
        return;
    };
    let start = chrono::DateTime::parse_from_rfc3339("2026-09-01T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    // 同时刻请求按 ID 稳定排列；成功请求的中间错误不能额外占据列表行
    for (id, outcome) in [
        ("status_a", "succeeded"),
        ("status_b", "failed"),
        ("status_c", "incomplete"),
        ("status_d", "cancelled"),
        ("status_e", "running"),
        ("status_f", "succeeded"),
    ] {
        seed(&database.pool, id, start, &[]).await;
        sqlx::query("update model_requests set outcome=$2, provider_kind='openai', completed_at=case when $2='running' then null else completed_at end, input_tokens=case when $2='succeeded' then input_tokens else null end, output_tokens=case when $2='succeeded' then output_tokens else null end, total_tokens=case when $2='succeeded' then total_tokens else null end where id=$1")
            .bind(id).bind(outcome).execute(&database.pool).await.unwrap();
    }
    sqlx::query(
        r#"update model_requests set requested_model_id='model_status_b',request_observation_json=jsonb_set(request_observation_json,'{error}','{"providerErrorCode":"server_error","message":"The upstream service is temporarily unavailable."}'::jsonb),client_status_code=502 where id='status_b'"#,
    )
    .execute(&database.pool)
    .await
    .unwrap();
    for id in ["event_a", "event_b"] {
        sqlx::query("insert into ops_events(id,model_request_id,attempt_index,level,component,operation,failure_kind,message,created_at) values($1,'status_a',1,'error','provider','request','upstream','retry evidence',$2)")
            .bind(id).bind(start).execute(&database.pool).await.unwrap();
    }
    sqlx::query("insert into ops_events(id,level,component,operation,failure_kind,message,created_at) values('standalone','error','system','refresh','upstream','standalone error',$1)").bind(start).execute(&database.pool).await.unwrap();
    super::drain_analytics(&database.pool).await;
    let repo = observability_repository(&database.pool);
    let range =
        ObservabilityRange::new(start - TimeDelta::minutes(1), start + TimeDelta::minutes(5))
            .unwrap();
    for (status, expected) in [
        (UsageRecordStatus::Success, vec!["status_f", "status_a"]),
        (UsageRecordStatus::Error, vec!["status_c", "status_b"]),
        (
            UsageRecordStatus::All,
            vec![
                "status_f", "status_e", "status_d", "status_c", "status_b", "status_a",
            ],
        ),
    ] {
        let mut actual = Vec::new();
        for current_page in 1..=4 {
            let page = repo
                .list_usage_records(UsageRecordQuery {
                    status,
                    range,
                    filter: UsageRecordFilter::default(),
                    current_page,
                    page_size: ObservabilityPageSize::new(2).unwrap(),
                })
                .await
                .unwrap();
            assert_eq!(page.total, expected.len() as u64);
            for item in page.items {
                if matches!(
                    item.outcome.as_str(),
                    "failed" | "incomplete" | "running" | "cancelled"
                ) {
                    assert_eq!(item.total_tokens, None);
                }
                actual.push(item.id);
            }
        }
        assert_eq!(actual, expected, "{status:?}");
    }
    let page = repo
        .list_usage_records(UsageRecordQuery {
            status: UsageRecordStatus::Error,
            range,
            filter: UsageRecordFilter {
                search: Some("model_status_b".into()),
                provider_kind: Some("openai".into()),
                ..Default::default()
            },
            current_page: 1,
            page_size: ObservabilityPageSize::new(10).unwrap(),
        })
        .await
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].id, "status_b");
    assert_eq!(page.items[0].client_status_code, Some(502));
    assert_eq!(page.items[0].error_code.as_deref(), Some("server_error"));
    assert_eq!(
        page.items[0].error_message.as_deref(),
        Some("The upstream service is temporarily unavailable.")
    );
    // 诊断包仍可按请求 ID 关联中间事件，不受列表时间范围或最终结果限制
    let errors = super::admin_observability_store(&database.pool)
        .diagnostic_errors(&DiagnosticTarget::Request("status_a".into()), 100)
        .await
        .unwrap();
    assert_eq!(errors.len(), 2);
    database.close().await;
}

#[tokio::test]
#[ignore = "Explicit 52k-row / 1GiB synthetic query rehearsal"]
async fn usage_scale_rehearsal_covers_pages_filters_details_and_error_summary() {
    use gateway_admin::{
        model::observability::{TimeRange, UsageRecordStatus},
        ports::store::ObservabilityStore as _,
    };
    use gateway_core::time::DeploymentTimeZone;
    use std::time::Instant;

    let database = TestDatabase::create("usage_scale")
        .await
        .expect("scale rehearsal requires isolated PostgreSQL");
    let start = Utc::now() - TimeDelta::days(7);
    seed(&database.pool, "scale_seed", start, &[]).await;
    // 合成大字段只扩大物理体积，不加入列表投影；禁止压缩避免重复夹具虚报规模
    sqlx::query(
        "alter table model_requests alter column provider_observation_json set storage external",
    )
    .execute(&database.pool)
    .await
    .unwrap();
    sqlx::query(r#"
        with padding as materialized (
          select jsonb_build_object('syntheticPadding',string_agg(md5(n::text),'')) payload
          from generate_series(1,640)n
        )
        insert into model_requests select cloned.*
        from model_requests original cross join generate_series(1,52000)n cross join padding
        cross join lateral jsonb_populate_record(null::model_requests,to_jsonb(original)||jsonb_build_object(
          'id','req_scale_'||lpad(n::text,5,'0'),
          'started_at',$1::timestamptz+n*interval '10 seconds',
          'completed_at',$1::timestamptz+n*interval '10 seconds'+interval '1 minute',
          'recovery_lease_expires_at',$1::timestamptz+n*interval '10 seconds'+interval '2 minutes',
          'downstream_committed_at',case when n%100=0 then null else $1::timestamptz+n*interval '10 seconds'+interval '1 minute' end,
          'provider_kind','openai','provider_account_ref','acct_scale',
          'attempt_count',case when n%200=0 then 0 else 1 end,
          'upstream_send_state',case when n%200=0 then 'not_sent' else 'sent' end,
          'upstream_transport',case when n%200=0 then null else 'http_sse' end,
          'outcome',case when n%100=0 then 'failed' else 'succeeded' end,
          'client_status_code',case when n%100=0 then 503 else 200 end,
          'upstream_status_code',case when n%200=0 then null when n%100=0 then 503 else 200 end,
          'error_kind',case when n%200=0 then 'provider_infrastructure_unavailable' when n%100=0 then 'upstream_error' else null end,
          'provider_observation_json',padding.payload,
          'request_observation_json',original.request_observation_json||jsonb_build_object('error',jsonb_build_object(
            'providerErrorCode',case when n%200=0 then 'infrastructure_unavailable' when n%100=0 then 'server_overloaded' else null end))
        )) cloned where original.id='scale_seed'
    "#).bind(start).execute(&database.pool).await.unwrap();
    sqlx::query("delete from model_requests where id='scale_seed'")
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("analyze model_requests")
        .execute(&database.pool)
        .await
        .unwrap();
    let physical_bytes: i64 = sqlx::query_scalar("select pg_total_relation_size('model_requests')")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert!(
        physical_bytes > 1_000_000_000,
        "physical bytes: {physical_bytes}"
    );
    super::drain_analytics(&database.pool).await;
    let range = ObservabilityRange::new(start, start + TimeDelta::days(7)).unwrap();
    let repository = observability_repository(&database.pool);
    let mut elapsed = serde_json::Map::new();
    for (label, status, current_page, filter, expected_total) in [
        (
            "default_first_page",
            UsageRecordStatus::Success,
            1,
            UsageRecordFilter::default(),
            51_480,
        ),
        (
            "all_first_page",
            UsageRecordStatus::All,
            1,
            UsageRecordFilter::default(),
            52_000,
        ),
        (
            "failed_window",
            UsageRecordStatus::Error,
            1,
            UsageRecordFilter::default(),
            520,
        ),
        (
            "deep_page_1040",
            UsageRecordStatus::All,
            1040,
            UsageRecordFilter::default(),
            52_000,
        ),
        (
            "name_and_model",
            UsageRecordStatus::All,
            1,
            UsageRecordFilter {
                client_api_key_name: Some("历史".into()),
                model_contains: Some("model_a".into()),
                ..Default::default()
            },
            52_000,
        ),
        (
            "request_id",
            UsageRecordStatus::All,
            1,
            UsageRecordFilter {
                request_id: Some("req_scale_00100".into()),
                ..Default::default()
            },
            1,
        ),
    ] {
        let began = Instant::now();
        let page = repository
            .list_usage_records(UsageRecordQuery {
                status,
                range,
                filter,
                current_page,
                page_size: ObservabilityPageSize::new(50).unwrap(),
            })
            .await
            .unwrap();
        elapsed.insert(label.into(), serde_json::json!(began.elapsed().as_millis()));
        assert_eq!(page.total, expected_total, "{label}");
        assert_eq!(
            page.items.len(),
            usize::min(expected_total as usize, 50),
            "{label}"
        );
        if current_page == 1040 {
            assert_eq!(page.items.first().unwrap().id, "req_scale_00050");
            assert_eq!(page.items.last().unwrap().id, "req_scale_00001");
        }
    }
    let began = Instant::now();
    let detail = repository
        .usage_record_detail("req_scale_00100")
        .await
        .unwrap();
    elapsed.insert(
        "single_detail".into(),
        serde_json::json!(began.elapsed().as_millis()),
    );
    assert_eq!(detail.request.id, "req_scale_00100");
    let began = Instant::now();
    let errors = repository
        .list_ops_errors(OpsErrorQuery {
            range,
            filter: OpsErrorFilter::default(),
            current_page: 1,
            page_size: ObservabilityPageSize::new(50).unwrap(),
        })
        .await
        .unwrap();
    elapsed.insert(
        "error_first_page".into(),
        serde_json::json!(began.elapsed().as_millis()),
    );
    assert_eq!(errors.total, 520);
    assert_eq!(errors.items.len(), 50);
    let began = Instant::now();
    let summary = super::admin_observability_store(&database.pool)
        .server_error_summary(
            TimeRange::new(range.start, range.end).unwrap(),
            DeploymentTimeZone::default(),
        )
        .await
        .unwrap();
    elapsed.insert(
        "server_family_summary".into(),
        serde_json::json!(began.elapsed().as_millis()),
    );
    assert_eq!(summary.totals.retained_requests, 52_000);
    assert_eq!(summary.totals.server_related_requests, 520);
    assert_eq!(summary.totals.overloaded, 260);
    assert_eq!(summary.totals.infrastructure503, 260);
    eprintln!(
        "TASK35_SCALE {}",
        serde_json::json!({"rows":52_000,"model_requests_physical_bytes":physical_bytes,"milliseconds":elapsed,"data":"synthetic; not a production snapshot","source_commit":option_env!("GITHUB_SHA")})
    );
    database.close().await;
}
