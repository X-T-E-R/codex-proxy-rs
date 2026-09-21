use std::{str::FromStr, time::Duration};

use gateway_store::postgres::{
    ObservabilityQueryBudget, PgAdminAccountStore, PgAdminObservabilityStore,
    PgObservabilityRepository, connect_and_migrate,
};
use sqlx::{
    ConnectOptions as _, PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use uuid::Uuid;

mod account_groups;
mod admin_security_audit;
mod admission_recovery;
mod backup;
mod client_budgets;
mod client_keys;
mod execution;
mod execution_buffer;
mod health;
mod observability;
mod ops_events;
mod pricing;
mod provider_accounts;
mod proxies;
mod query_budget;
mod retention;
mod runtime_settings;
mod schema_integrity;
mod snapshot;
mod snapshots;
mod turn_state;

static TEST_MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

const DEPLOYED_FORK_MIGRATIONS: &[(i64, &str, &str)] = &[
    (
        1,
        "initial",
        "66daf12a89a1ec11a81110c1ce6289f110bf65beaa432f2668a2bddc6fd246933138dba2d70a60a3d29c6aa6b2e631c1",
    ),
    (
        2,
        "client key budgets",
        "9c29e6c73340926f312b58af3b58b5be43b021ccc77797b4ebbf016cac6da802948d99f8e4a324cd1113184698e7ae8a",
    ),
    (
        3,
        "account outbound proxy",
        "8a0ed3ad9db3132dfefe7bc60dc78558c6e1fc687575ec8f21c0a0e27c5aa0866fd1fbdaaa66a6ad82be0816ae41efc8",
    ),
    (
        4,
        "remove client charge reconciliation",
        "5379ecf2efdcc4c68c48a083284f38cef3be3e27cba46bf86d919649ab1622b4f86e4bbdae031758da1db56b2dd73860",
    ),
    (
        5,
        "managed outbound proxies",
        "a0691a7cb853a9e03abc94b0e3564384dedecfe8e6b68deaba4d059a7a9863764b28daa6dadacf24b53775c8cdeaa9b6",
    ),
    (
        6,
        "ws pool runtime settings",
        "6d08edfc0b1ea7c2a13afa5ac318fb89f3dea6a91608918c27f1c86c7b3b1a515a43e43cbfd4cd09a1b74f86d9172581",
    ),
    (
        7,
        "overload cooldown",
        "6e1dd276f7f462fcd6f33299059b479e2f7a03b5d1f6abac78526c63fcc3546c61191cba62f91684e1450f11a6a6f2d7",
    ),
    (
        8,
        "openai user agent",
        "85d41a20952cc776730ff3bd54f344e093895150b13b1fd3311694bd54e097bfb53197a4d248e576635c9eb380f5d68c",
    ),
    (
        9,
        "cyber session block",
        "3639a56c6b43534e820814d14a985fda593241e341c4efb755c5eec3af233287a6542a90e3de907969a5f92e63d658fd",
    ),
    (
        10,
        "openai request locale",
        "1eb2db277349cd1fea31adea40f26cd06c14e19061609ee29461df8aeb6b299e0af230d6b00eda434aed6b6227c38be3",
    ),
    (
        11,
        "openai turn state",
        "7fffc53921126a187f4ced497916708aaa9c36e50135fb834a2a6655d4acc3c769b8cf55f65d4148e7c2b75d4b474af1",
    ),
    (
        12,
        "request turn state",
        "2f4243ffa1db5b3f9b18d1e12c5b84a18ef339ce798aa2ad8ba628f413424578a723c3598577b95ebadb8970fcdec799",
    ),
    (
        13,
        "openai model turn state",
        "0187b793d941d3ca14b646eeacfacb8efaf0c8a090d15de5d4807532d3c9416ce8f62f0aa1ba4822a417c19a196bf999",
    ),
    (
        14,
        "openai account turn state policy",
        "49d08f8e9f176f982bbaebee48e99d0e986a13425e270005c403159f2a91c4b70fa4ada5a9c5fa52dd31e4dcdcff825c",
    ),
    (
        15,
        "openai turn state rotation",
        "31592a8a58a8c51b15fc437e692e2c9cd2f73ccaa7fa29a0420bcb4f528461e10b419bc7898e97bcc595953d6ee05816",
    ),
    (
        16,
        "openai turn state capture modes",
        "54ef952e48d3b077a024f381fa125772ca9ad143bd13073a28ccd2f672d1d68c0aa999cade3f6cc8d975ea2921142eb4",
    ),
    (
        17,
        "openai turn state missing action",
        "2a5fb4d567f7e960bcd03ce453019bb08509e9443a6ea81655de76ced5663704c59bcf5485dad3c15ab29cf602a2b781",
    ),
    (
        18,
        "openai turn state capture attempt limit",
        "c2359f68b0dcef276594a083f8e775a0c9a6c1ee940e830ba4ed9c82c05e8c18375059ae0cf1053e8c7dff13d904bda9",
    ),
    (
        19,
        "account model access and capacity queue",
        "057f756f06cd28dc2d93f2c8b929c53d167dfa79805b14288551e5bb9023725f96b7cb4e1ee6baa82f79afae815b788e",
    ),
];

pub(super) struct TestDatabase {
    admin: PgPool,
    pub(super) pool: PgPool,
    schema: String,
}

pub(super) fn observability_query_budget() -> ObservabilityQueryBudget {
    ObservabilityQueryBudget::try_new(4, Duration::from_secs(1))
        .expect("valid test observability query budget")
}

pub(super) fn observability_repository(pool: &PgPool) -> PgObservabilityRepository {
    PgObservabilityRepository::new(pool.clone(), None, observability_query_budget())
}

pub(super) fn admin_observability_store(pool: &PgPool) -> PgAdminObservabilityStore {
    PgAdminObservabilityStore::new(pool.clone(), None, None, observability_query_budget())
}

pub(super) fn admin_account_store(pool: &PgPool) -> PgAdminAccountStore {
    PgAdminAccountStore::new(pool.clone(), None, observability_query_budget())
}

impl TestDatabase {
    pub(super) async fn create(label: &str) -> Option<Self> {
        let database_url = crate::support::test_env("CPR_TEST_DATABASE_URL")?;
        let schema = format!("cpr_store_{label}_{}", Uuid::new_v4().simple());
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&database_url)
            .await
            .expect("connect test PostgreSQL");
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("create schema \"{schema}\"")))
            .execute(&admin)
            .await
            .expect("create test schema");
        let search_path = schema.clone();
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .after_connect(move |connection, _metadata| {
                let search_path = search_path.clone();
                Box::pin(async move {
                    sqlx::query("select set_config('search_path', $1, false)")
                        .bind(search_path)
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect(&database_url)
            .await
            .expect("connect isolated test schema");
        TEST_MIGRATOR
            .run(&pool)
            .await
            .expect("apply test migrations");
        Some(Self {
            admin,
            pool,
            schema,
        })
    }

    pub(super) async fn close(self) {
        self.pool.close().await;
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "drop schema \"{}\" cascade",
            self.schema
        )))
        .execute(&self.admin)
        .await
        .expect("drop test schema");
        self.admin.close().await;
    }
}

#[tokio::test]
async fn connect_and_migrate_should_apply_all_migrations_once_and_reopen_cleanly() {
    let Some(database_url) = crate::support::test_env("CPR_TEST_DATABASE_URL") else {
        return;
    };
    let database = format!("cpr_store_migrator_{}", Uuid::new_v4().simple());
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("connect migration test PostgreSQL");
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "create database \"{database}\""
    )))
    .execute(&admin)
    .await
    .expect("create migration test database");

    let isolated_url = PgConnectOptions::from_str(&database_url)
        .expect("parse migration test PostgreSQL URL")
        .database(&database)
        .to_url_lossy()
        .to_string();
    let pool_config = gateway_store::StorePoolConfig::default();
    let first = connect_and_migrate(&isolated_url, pool_config)
        .await
        .expect("apply migrations through production migrator");
    let session_settings = sqlx::query_as::<_, (String, i64, i64, i64)>(
        "select current_setting('application_name'),
                extract(epoch from current_setting('statement_timeout')::interval)::bigint,
                extract(epoch from current_setting('lock_timeout')::interval)::bigint,
                extract(epoch from current_setting(
                    'idle_in_transaction_session_timeout'
                )::interval)::bigint",
    )
    .fetch_one(&first)
    .await
    .expect("load runtime PostgreSQL session settings");
    let first_tables = sqlx::query_scalar::<_, String>(
        "select table_name
         from information_schema.tables
         where table_schema = 'public'
         order by table_name",
    )
    .fetch_all(&first)
    .await
    .expect("load migrated tables");
    first.close().await;

    let second = connect_and_migrate(&isolated_url, pool_config)
        .await
        .expect("reopen database through production migrator");
    let migration_count =
        sqlx::query_scalar::<_, i64>("select count(*) from _sqlx_migrations where success")
            .fetch_one(&second)
            .await
            .expect("count successful migrations");
    let response_id_types = sqlx::query_scalar::<_, String>(
        "select data_type
         from information_schema.columns
         where table_schema = 'public'
           and table_name = 'model_requests'
           and column_name in ('client_response_id', 'upstream_response_id')
         order by column_name",
    )
    .fetch_all(&second)
    .await
    .expect("load opaque response ID column types");
    let raw_response_id_index_exists = sqlx::query_scalar::<_, bool>(
        "select exists (
           select 1
           from pg_indexes
           where schemaname = 'public'
             and indexname = 'model_requests_client_response_uq'
         )",
    )
    .fetch_one(&second)
    .await
    .expect("check removed raw response ID index");
    let legacy_key_provider_column_exists = sqlx::query_scalar::<_, bool>(
        "select exists (
           select 1 from information_schema.columns
           where table_schema = 'public'
             and table_name = 'client_api_keys'
             and column_name = 'provider_kind'
         )",
    )
    .fetch_one(&second)
    .await
    .expect("check removed client key provider column");
    let routing_history_columns = sqlx::query_scalar::<_, String>(
        "select column_name from information_schema.columns
         where table_schema = 'public'
           and table_name = 'model_requests'
           and column_name in (
             'routing_scope', 'routing_group_refs', 'routing_group_names_snapshot'
           )
         order by column_name",
    )
    .fetch_all(&second)
    .await
    .expect("load routing history columns");
    let legacy_model_policy_columns = sqlx::query_scalar::<_, String>(
        "select column_name from information_schema.columns
         where table_schema = 'public'
           and table_name = 'openai_model_turn_states'
           and column_name in (
             'lock_enabled', 'capture_enabled', 'reuse_window_seconds',
             'capture_proxy_id', 'max_attempts', 'attempt_timeout_seconds',
             'job_timeout_seconds', 'backoff_seconds', 'max_backoff_seconds',
             'cooldown_seconds'
           )
         order by column_name",
    )
    .fetch_all(&second)
    .await
    .expect("check removed model policy columns");
    second.close().await;

    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "drop database \"{database}\" with (force)"
    )))
    .execute(&admin)
    .await
    .expect("drop migration test database");
    admin.close().await;

    assert_eq!(
        first_tables,
        [
            "_sqlx_migrations",
            "account_group_accounts",
            "account_groups",
            "admin_audit_events",
            "admin_users",
            "backup_records",
            "backup_settings",
            "client_api_key_groups",
            "client_api_keys",
            "client_key_budget_windows",
            "client_key_charge_events",
            "model_requests",
            "openai_account_turn_state_policies",
            "openai_model_turn_states",
            "openai_turn_states",
            "ops_events",
            "outbound_proxies",
            "provider_accounts",
            "request_turn_state_observations",
            "runtime_settings",
        ]
    );
    assert_eq!(session_settings, ("codex-proxy-rs".to_owned(), 30, 5, 30));
    assert_eq!(
        migration_count,
        i64::try_from(TEST_MIGRATOR.iter().count())
            .expect("migration count fits PostgreSQL bigint")
    );
    assert_eq!(response_id_types, ["bytea", "bytea"]);
    assert!(!raw_response_id_index_exists);
    assert!(!legacy_key_provider_column_exists);
    assert_eq!(
        routing_history_columns,
        [
            "routing_group_names_snapshot",
            "routing_group_refs",
            "routing_scope",
        ]
    );
    assert!(legacy_model_policy_columns.is_empty());
}

#[test]
fn migrations_should_leave_transaction_ownership_to_sqlx() {
    let transaction_statements = TEST_MIGRATOR
        .iter()
        .flat_map(|migration| migration.sql.as_str().lines())
        .map(str::trim)
        .filter(|line| matches!(*line, "begin;" | "commit;"))
        .count();

    assert_eq!(transaction_statements, 0);
}

#[test]
fn deployed_fork_migration_prefix_should_remain_byte_compatible() {
    let migrations = TEST_MIGRATOR.iter().collect::<Vec<_>>();
    for (index, migration) in migrations.iter().enumerate() {
        assert_eq!(migration.version, i64::try_from(index + 1).unwrap());
    }
    for (migration, (version, description, checksum)) in migrations
        .iter()
        .take(DEPLOYED_FORK_MIGRATIONS.len())
        .zip(DEPLOYED_FORK_MIGRATIONS)
    {
        assert_eq!(migration.version, *version);
        assert_eq!(migration.description.as_ref(), *description);
        assert_eq!(hex::encode(migration.checksum.as_ref()), *checksum);
    }
    assert!(migrations.len() > DEPLOYED_FORK_MIGRATIONS.len());
}
