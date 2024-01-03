use crate::reconciler::ReconcileError;
use crate::service::Service;
use hyper::StatusCode;
use hyper::{Body, Request, Response};
use pageserver_api::models::{TenantCreateRequest, TimelineCreateRequest};
use pageserver_api::shard::TenantShardId;
use std::sync::Arc;
use utils::http::endpoint::request_span;
use utils::http::request::parse_request_param;
use utils::id::TenantId;

use utils::{
    http::{
        endpoint::{self},
        error::ApiError,
        json::{json_request, json_response},
        RequestExt, RouterBuilder,
    },
    id::NodeId,
};

use pageserver_api::control_api::{ReAttachRequest, ValidateRequest};

use control_plane::attachment_service::{
    AttachHookRequest, InspectRequest, NodeConfigureRequest, NodeRegisterRequest,
    TenantShardMigrateRequest,
};

/// State available to HTTP request handlers
#[derive(Clone)]
pub struct HttpState {
    service: Arc<crate::service::Service>,
}

impl HttpState {
    pub fn new(service: Arc<crate::service::Service>) -> Self {
        Self { service }
    }
}

#[inline(always)]
fn get_state(request: &Request<Body>) -> &HttpState {
    request
        .data::<Arc<HttpState>>()
        .expect("unknown state type")
        .as_ref()
}

/// Pageserver calls into this on startup, to learn which tenants it should attach
async fn handle_re_attach(mut req: Request<Body>) -> Result<Response<Body>, ApiError> {
    let reattach_req = json_request::<ReAttachRequest>(&mut req).await?;
    let state = get_state(&req);
    json_response(StatusCode::OK, state.service.re_attach(reattach_req))
}

/// Pageserver calls into this before doing deletions, to confirm that it still
/// holds the latest generation for the tenants with deletions enqueued
async fn handle_validate(mut req: Request<Body>) -> Result<Response<Body>, ApiError> {
    let validate_req = json_request::<ValidateRequest>(&mut req).await?;
    let state = get_state(&req);
    json_response(StatusCode::OK, state.service.validate(validate_req))
}

/// Call into this before attaching a tenant to a pageserver, to acquire a generation number
/// (in the real control plane this is unnecessary, because the same program is managing
///  generation numbers and doing attachments).
async fn handle_attach_hook(mut req: Request<Body>) -> Result<Response<Body>, ApiError> {
    let attach_req = json_request::<AttachHookRequest>(&mut req).await?;
    let state = get_state(&req);

    json_response(StatusCode::OK, state.service.attach_hook(attach_req))
}

async fn handle_inspect(mut req: Request<Body>) -> Result<Response<Body>, ApiError> {
    let inspect_req = json_request::<InspectRequest>(&mut req).await?;

    let state = get_state(&req);

    json_response(StatusCode::OK, state.service.inspect(inspect_req))
}

async fn handle_tenant_create(mut req: Request<Body>) -> Result<Response<Body>, ApiError> {
    let create_req = json_request::<TenantCreateRequest>(&mut req).await?;
    let state = get_state(&req);
    json_response(
        StatusCode::OK,
        state.service.tenant_create(create_req).await?,
    )
}

async fn handle_tenant_timeline_create(mut req: Request<Body>) -> Result<Response<Body>, ApiError> {
    let tenant_id: TenantId = parse_request_param(&req, "tenant_id")?;
    let create_req = json_request::<TimelineCreateRequest>(&mut req).await?;

    let state = get_state(&req);
    json_response(
        StatusCode::OK,
        state
            .service
            .tenant_timeline_create(tenant_id, create_req)
            .await?,
    )
}

async fn handle_tenant_locate(req: Request<Body>) -> Result<Response<Body>, ApiError> {
    let tenant_id: TenantId = parse_request_param(&req, "tenant_id")?;
    let state = get_state(&req);

    json_response(StatusCode::OK, state.service.tenant_locate(tenant_id)?)
}

async fn handle_node_register(mut req: Request<Body>) -> Result<Response<Body>, ApiError> {
    let register_req = json_request::<NodeRegisterRequest>(&mut req).await?;
    let state = get_state(&req);
    state.service.node_register(register_req);
    json_response(StatusCode::OK, ())
}

async fn handle_node_configure(mut req: Request<Body>) -> Result<Response<Body>, ApiError> {
    let node_id: NodeId = parse_request_param(&req, "node_id")?;
    let config_req = json_request::<NodeConfigureRequest>(&mut req).await?;
    if node_id != config_req.node_id {
        return Err(ApiError::BadRequest(anyhow::anyhow!(
            "Path and body node_id differ"
        )));
    }
    let state = get_state(&req);

    json_response(StatusCode::OK, state.service.node_configure(config_req)?)
}

async fn handle_tenant_shard_migrate(mut req: Request<Body>) -> Result<Response<Body>, ApiError> {
    let tenant_shard_id: TenantShardId = parse_request_param(&req, "tenant_shard_id")?;
    let migrate_req = json_request::<TenantShardMigrateRequest>(&mut req).await?;
    let state = get_state(&req);
    json_response(
        StatusCode::OK,
        state
            .service
            .tenant_shard_migrate(tenant_shard_id, migrate_req)
            .await?,
    )
}

/// Status endpoint is just used for checking that our HTTP listener is up
async fn handle_status(_req: Request<Body>) -> Result<Response<Body>, ApiError> {
    json_response(StatusCode::OK, ())
}

impl From<ReconcileError> for ApiError {
    fn from(value: ReconcileError) -> Self {
        ApiError::Conflict(format!("Reconciliation error: {}", value))
    }
}

pub fn make_router(service: Arc<Service>) -> RouterBuilder<hyper::Body, ApiError> {
    endpoint::make_router()
        .data(Arc::new(HttpState { service }))
        .get("/status", |r| request_span(r, handle_status))
        .post("/re-attach", |r| request_span(r, handle_re_attach))
        .post("/validate", |r| request_span(r, handle_validate))
        .post("/attach-hook", |r| request_span(r, handle_attach_hook))
        .post("/inspect", |r| request_span(r, handle_inspect))
        .post("/node", |r| request_span(r, handle_node_register))
        .put("/node/:node_id/config", |r| {
            request_span(r, handle_node_configure)
        })
        .post("/tenant", |r| request_span(r, handle_tenant_create))
        .post("/tenant/:tenant_id/timeline", |r| {
            request_span(r, handle_tenant_timeline_create)
        })
        .get("/tenant/:tenant_id/locate", |r| {
            request_span(r, handle_tenant_locate)
        })
        .put("/tenant/:tenant_shard_id/migrate", |r| {
            request_span(r, handle_tenant_shard_migrate)
        })
}
