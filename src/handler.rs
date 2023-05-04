use std::{collections::HashMap, net::SocketAddr};

use axum::{
    extract::{ConnectInfo, Path, Query, State},
    response::IntoResponse,
};
use busylib::{
    logger::change_debug,
    prelude::{EnhancedExpect, EnhancedUnwrap},
};
use http::{Response, StatusCode};
use hyper::Body;
use log::debug;
use piam_core::{
    account::aws::AwsAccount,
    condition::input::{Condition, ConditionCtx},
};
use piam_object_storage::{input::ObjectStorageInput, policy::ObjectStoragePolicy};
use piam_proxy::{
    container::{FoundPolicies, IamContainer, PolicyFilterParams},
    error::ProxyResult,
    policy::FindEffect,
    request::{forward, AccessTarget, HttpRequestExt},
    response::HttpResponseExt,
    signature::{
        aws::{AwsSigv4, AwsSigv4SignParams},
        SigHeader,
    },
    state::ArcState,
    type_alias::{HttpRequest, HttpResponse},
};

use crate::{
    config::SERVICE, error::from_parser_into_proxy_error, request::S3RequestTransform, S3Config,
};

pub type S3ProxyState = ArcState<ObjectStoragePolicy, S3Config>;

pub async fn health() -> impl IntoResponse {
    "OK"
}

pub async fn manage(
    State(state): State<S3ProxyState>,
    Query(params): Query<HashMap<String, String>>,
    // mut req: Request<Body>,
) -> HttpResponse {
    // TODO: turn debug mode on/off
    fn resp(payload: &str) -> HttpResponse {
        Response::builder()
            .body(Body::from(payload.to_string()))
            .unwp()
    }
    if let Some(debug) = params.get("debug") {
        let on = change_debug(state.load().log_handle.as_ref().unwp(), debug.as_str());
        return if on {
            resp("debug mode on")
        } else {
            resp("debug mode off")
        };
    }
    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .body(Body::from("invalid request"))
        .unwp()
}

pub async fn handle_path(
    Path(path): Path<String>,
    State(state): State<S3ProxyState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    mut req: HttpRequest,
) -> ProxyResult<HttpResponse> {
    let proxy_hosts = &state.load().extended_config.proxy_hosts.domains;
    req.adapt_path_style(path, proxy_hosts)?;
    handle(State(state), ConnectInfo(addr), req).await
}

pub async fn handle(
    State(state): State<S3ProxyState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: HttpRequest,
) -> ProxyResult<HttpResponse> {
    log(&req);
    req.validate()?;

    let state = state.load();
    let s3_config = &state.extended_config;
    let iam_container = &state.iam_container;

    let (input, req) = ObjectStorageInput::parse(req, &s3_config.proxy_hosts)
        .await
        .map_err(from_parser_into_proxy_error)?
        .into_parts();

    let (access_target, base_access_key) =
        get_access_params(iam_container, s3_config, &input, &req)?;
    let policies = find_matching_policies(&access_target, &base_access_key, iam_container)?;
    let req = apply_policies_to_req(addr, &input, policies, req)?;
    let signed_req = sign(s3_config, access_target, req).await?;
    let res = forward(signed_req, &state.http_client).await?;
    Ok(res.add_piam_headers_with_random_id())
}

fn log(req: &HttpRequest) {
    debug!("req.uri '{}'", req.uri());
    debug!("req.method {}", req.method());
    debug!("req.headers {:#?}", req.headers());
}

fn get_access_params(
    iam_container: &IamContainer<ObjectStoragePolicy>,
    s3_config: &S3Config,
    input: &ObjectStorageInput,
    req: &HttpRequest,
) -> ProxyResult<(AccessTarget, String)> {
    // aws sigv4 specific
    #[allow(unused)]
    let (access_key, region) = req.extract_access_key_and_region()?;
    // When feature uni-key is enabled, base_access_key is aws access_key,
    // otherwise base_access_key + account_code = aws_access_key
    #[cfg(feature = "uni-key")]
    let (access_target, base_access_key) = {
        let access_info = s3_config
            .get_uni_key_info()?
            .find_access_info(&input, region)?;
        (
            AccessTarget {
                account: access_info.account.clone(),
                region: access_info.region.clone(),
            },
            access_key,
        )
    };
    #[cfg(not(feature = "uni-key"))]
    let (access_target, base_access_key) = {
        use piam_proxy::signature::split_to_base_and_account_code;
        let (base_access_key, code) = split_to_base_and_account_code(access_key)?;
        let account = iam_container.find_account_by_code(code)?;
        (
            AccessTarget {
                account: account.clone(),
                region: region.to_string(),
            },
            base_access_key,
        )
    };
    Ok((access_target, base_access_key.to_string()))
}

fn find_matching_policies<'a>(
    access_target: &AccessTarget,
    base_access_key: &str,
    iam_container: &'a IamContainer<ObjectStoragePolicy>,
) -> ProxyResult<FoundPolicies<'a, ObjectStoragePolicy>> {
    let user = iam_container.find_user_by_base_access_key(base_access_key)?;
    let groups = iam_container.find_groups_by_user(user)?;
    let policy_filter_param =
        PolicyFilterParams::new_with(&access_target.account, &access_target.region).groups(&groups);
    let policies = iam_container.find_policies(&policy_filter_param)?;
    Ok(policies)
}

fn apply_policies_to_req(
    addr: SocketAddr,
    input: &ObjectStorageInput,
    policies: FoundPolicies<ObjectStoragePolicy>,
    req: HttpRequest,
) -> ProxyResult<HttpRequest> {
    let condition_ctx = ConditionCtx::default().from(Condition::new_with_addr(addr));
    let condition_effects = policies.condition.find_effects(&condition_ctx)?;
    let req = req.apply_effects(condition_effects)?;

    let user_input_effects = policies.user_input.find_effects(&input)?;
    let mut req = req.apply_effects(user_input_effects)?;
    Ok(req)
}

async fn sign(
    s3_config: &S3Config,
    access_target: AccessTarget,
    mut req: HttpRequest,
) -> ProxyResult<HttpRequest> {
    req.set_actual_host(s3_config, &access_target.region)?;
    let sign_params =
        AwsSigv4SignParams::new_with(&access_target.account, SERVICE, &access_target.region);
    let signed_req = req
        .sign_with_aws_sigv4_params(&sign_params)
        .await
        .ex("sign should not fail");
    Ok(signed_req)
}
