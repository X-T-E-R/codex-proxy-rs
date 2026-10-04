//! 全局默认与账号额度策略的管理员 HTTP 边界。

use super::*;
use crate::auth::SessionState;
use gateway_core::provider_ports::quota_policy::{QuotaPolicy, QuotaPolicyMode};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GlobalUpdate {
    expected_revision: u64,
    policy: QuotaPolicy,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AccountUpdate {
    account_id: String,
    expected_revision: u64,
    mode: QuotaPolicyMode,
    policy: Option<QuotaPolicy>,
}

pub(super) fn router<S>() -> Router<S>
where
    S: SessionState + Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/api/admin/quota-policy", get(global::<S>))
        .route("/api/admin/quota-policy/update", post(update_global::<S>))
        .route("/api/admin/accounts/quota-policy", get(account::<S>))
        .route(
            "/api/admin/accounts/quota-policy/update",
            post(update_account::<S>),
        )
}

async fn global<S>(
    _auth: AdminAuth,
    State(state): State<S>,
) -> Result<impl IntoResponse, AdminError>
where
    S: SessionState + Send + Sync,
{
    let view = state
        .admin_services()
        .accounts()
        .global_quota_policy()
        .await
        .map_err(map_service_error)?;
    Ok(AdminResponse::new(StatusCode::OK, AdminEnvelope::ok(view)))
}

async fn update_global<S>(
    auth: AdminAuth,
    State(state): State<S>,
    AdminJson(request): AdminJson<GlobalUpdate>,
) -> Result<impl IntoResponse, AdminError>
where
    S: SessionState + Send + Sync,
{
    let view = state
        .admin_services()
        .accounts()
        .update_global_quota_policy(
            request.expected_revision,
            request.policy,
            &auth.context().mutation_context(),
        )
        .await
        .map_err(map_service_error)?;
    Ok(AdminResponse::new(StatusCode::OK, AdminEnvelope::ok(view)))
}

async fn account<S>(
    _auth: AdminAuth,
    State(state): State<S>,
    AdminQuery(query): AdminQuery<AccountIdQuery>,
) -> Result<impl IntoResponse, AdminError>
where
    S: SessionState + Send + Sync,
{
    let id = query.into_id().map_err(map_wire_error)?;
    let view = state
        .admin_services()
        .accounts()
        .account_quota_policy(&id)
        .await
        .map_err(map_service_error)?;
    Ok(AdminResponse::new(StatusCode::OK, AdminEnvelope::ok(view)))
}

async fn update_account<S>(
    auth: AdminAuth,
    State(state): State<S>,
    AdminJson(request): AdminJson<AccountUpdate>,
) -> Result<impl IntoResponse, AdminError>
where
    S: SessionState + Send + Sync,
{
    require_account_id(&request.account_id, "accountId").map_err(map_wire_error)?;
    let id = ProviderAccountId::new(request.account_id)
        .map_err(|_| map_wire_error(WireValidationError::new("accountId")))?;
    let view = state
        .admin_services()
        .accounts()
        .update_account_quota_policy(
            &id,
            request.expected_revision,
            request.mode,
            request.policy,
            &auth.context().mutation_context(),
        )
        .await
        .map_err(map_service_error)?;
    Ok(AdminResponse::new(StatusCode::OK, AdminEnvelope::ok(view)))
}
