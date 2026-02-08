use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::AppState;

enum UpstreamResult {
    Success { body: String, content_type: String },
    RateLimited,
    Error { status: StatusCode, body: String },
}

struct FallbackCredentials {
    api_key: String,
    site_id: String,
}

/// Make a single upstream request with explicit site_id and api_key.
async fn fetch_upstream(
    state: &AppState,
    site_id: &str,
    endpoint: &str,
    api_key: &str,
    params: &[(String, String)],
) -> Result<UpstreamResult, reqwest::Error> {
    let url = format!(
        "{}/rooftop_sites/{}/{}",
        state.upstream_url, site_id, endpoint
    );

    let mut req = state
        .client
        .get(&url)
        .header("Accept", "application/json")
        .bearer_auth(api_key);
    if !params.is_empty() {
        req = req.query(params);
    }

    let response = req.send().await?;
    let status = response.status();
    let content_type = response
        .headers()
        .get("Content-Type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();

    if status == StatusCode::TOO_MANY_REQUESTS {
        let rl_info = extract_rate_limit_headers(response.headers());
        tracing::warn!("{}/{}: upstream 429{}", site_id, endpoint, rl_info);
        return Ok(UpstreamResult::RateLimited);
    }

    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Ok(UpstreamResult::Error { status, body });
    }

    let rl_info = extract_rate_limit_headers(response.headers());
    if !rl_info.is_empty() {
        tracing::info!("{}/{}: upstream OK{}", site_id, endpoint, rl_info);
    }

    let body = response.text().await?;
    Ok(UpstreamResult::Success { body, content_type })
}

/// Try the fallback account. Returns Some(Response) on success, None if unavailable/failed.
async fn try_fallback(
    state: &AppState,
    fallback: &FallbackCredentials,
    rooftop_id: &str,
    endpoint: &str,
    cache_endpoint: &str,
    params: &[(String, String)],
) -> Option<Response> {
    let fb_rate_key = format!("fallback:{}", fallback.site_id);

    if !state.cache.can_fetch(&fb_rate_key, cache_endpoint, state.rate_limit).await {
        tracing::info!("{}/{}: fallback also rate limited", rooftop_id, endpoint);
        return None;
    }

    tracing::info!("{}/{}: primary 429, trying fallback site", rooftop_id, endpoint);
    state.cache.mark_attempt(&fb_rate_key, cache_endpoint).await;

    match fetch_upstream(state, &fallback.site_id, endpoint, &fallback.api_key, params).await {
        Ok(UpstreamResult::Success { body, content_type }) => {
            // Cache under the ORIGINAL site ID's key
            state
                .cache
                .set(rooftop_id, cache_endpoint, body.clone(), content_type.clone())
                .await;
            tracing::info!("{}/{}: FALLBACK (fetched {}B)", rooftop_id, endpoint, body.len());
            Some(cached_response(&body, &content_type, "FALLBACK", 0))
        }
        Ok(UpstreamResult::RateLimited) => {
            tracing::warn!("{}/{}: fallback also 429", rooftop_id, endpoint);
            state.cache.mark_failed_attempt(&fb_rate_key, cache_endpoint, state.rate_limit, 3600).await;
            None
        }
        Ok(UpstreamResult::Error { status, body }) => {
            tracing::error!("{}/{}: fallback error {} - {}", rooftop_id, endpoint, status, body);
            state.cache.mark_failed_attempt(&fb_rate_key, cache_endpoint, state.rate_limit, 60).await;
            None
        }
        Err(e) => {
            tracing::error!("{}/{}: fallback fetch failed: {}", rooftop_id, endpoint, e);
            state.cache.mark_failed_attempt(&fb_rate_key, cache_endpoint, state.rate_limit, 60).await;
            None
        }
    }
}

/// Extract fallback credentials from request headers.
fn extract_fallback(headers: &HeaderMap) -> Option<FallbackCredentials> {
    let api_key = headers.get("X-Fallback-Api-Key")?.to_str().ok()?.to_string();
    let site_id = headers.get("X-Fallback-Site-Id")?.to_str().ok()?.to_string();
    Some(FallbackCredentials { api_key, site_id })
}

/// Handle proxied requests to Solcast API with caching.
pub async fn proxy_handler(
    State(state): State<Arc<AppState>>,
    Path((rooftop_id, endpoint)): Path<(String, String)>,
    Query(params): Query<Vec<(String, String)>>,
    headers: HeaderMap,
) -> Response {
    // Validate endpoint
    if endpoint != "forecasts" && endpoint != "estimated_actuals" {
        return (StatusCode::NOT_FOUND, "Unknown endpoint").into_response();
    }

    // Build cache key including query params for uniqueness
    let cache_endpoint = if params.is_empty() {
        endpoint.clone()
    } else {
        let qs: Vec<String> = params.iter().map(|(k, v)| format!("{k}={v}")).collect();
        format!("{}?{}", endpoint, qs.join("&"))
    };

    // Cache-Control: no-cache bypasses both TTL and rate limit
    let force_refresh = headers
        .get("Cache-Control")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("no-cache"));

    // Check if cache is fresh (skipped on force refresh)
    if !force_refresh && state.cache.is_fresh(&rooftop_id, &cache_endpoint, state.ttl).await {
        if let Some((entry, age)) = state.cache.get(&rooftop_id, &cache_endpoint).await {
            tracing::info!("{}/{}: HIT (age {}s)", rooftop_id, endpoint, age);
            return cached_response(&entry.body, &entry.content_type, "HIT", age);
        }
    }

    if force_refresh {
        tracing::info!("{}/{}: cache bust requested", rooftop_id, endpoint);
    }

    let fallback = extract_fallback(&headers);

    // Cache is stale or missing — check rate limit (skipped on force refresh)
    if !force_refresh && !state.cache.can_fetch(&rooftop_id, &cache_endpoint, state.rate_limit).await {
        // Primary rate limited — try fallback before serving stale
        if let Some(fb) = &fallback {
            if let Some(resp) = try_fallback(&state, fb, &rooftop_id, &endpoint, &cache_endpoint, &params).await {
                state.cache.mark_failed_attempt(&rooftop_id, &cache_endpoint, state.rate_limit, 3600).await;
                return resp;
            }
        }

        // Fallback unavailable — serve stale if available
        if let Some((entry, age)) = state.cache.get(&rooftop_id, &cache_endpoint).await {
            tracing::info!("{}/{}: STALE (age {}s, rate limited)", rooftop_id, endpoint, age);
            return cached_response(&entry.body, &entry.content_type, "STALE", age);
        }
        tracing::warn!("{}/{}: rate limited, no cached data", rooftop_id, endpoint);
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("Retry-After", "9000")],
            "Rate limited and no cached data available",
        )
            .into_response();
    }

    // Extract API key from Authorization header
    let api_key = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");

    // Fetch upstream (mark attempt to prevent concurrent hammering; clear on failure)
    state.cache.mark_attempt(&rooftop_id, &cache_endpoint).await;
    tracing::info!("{}/{}: fetching upstream", rooftop_id, endpoint);

    match fetch_upstream(&state, &rooftop_id, &endpoint, api_key, &params).await {
        Ok(UpstreamResult::Success { body, content_type }) => {
            state
                .cache
                .set(&rooftop_id, &cache_endpoint, body.clone(), content_type.clone())
                .await;
            tracing::info!("{}/{}: MISS (fetched {}B)", rooftop_id, endpoint, body.len());
            cached_response(&body, &content_type, "MISS", 0)
        }
        Ok(UpstreamResult::RateLimited) => {
            // Primary returned 429 — try fallback
            if let Some(fb) = &fallback {
                if let Some(resp) = try_fallback(&state, fb, &rooftop_id, &endpoint, &cache_endpoint, &params).await {
                    state.cache.mark_failed_attempt(&rooftop_id, &cache_endpoint, state.rate_limit, 3600).await;
                    return resp;
                }
            }

            // Fallback unavailable or failed — fall through to stale cache
            state.cache.mark_failed_attempt(&rooftop_id, &cache_endpoint, state.rate_limit, 3600).await;
            if let Some((entry, age)) = state.cache.get(&rooftop_id, &cache_endpoint).await {
                return cached_response(&entry.body, &entry.content_type, "STALE", age);
            }
            (StatusCode::TOO_MANY_REQUESTS, "Upstream rate limited").into_response()
        }
        Ok(UpstreamResult::Error { status, body }) => {
            tracing::error!("{}/{}: upstream error {} - {}", rooftop_id, endpoint, status, body);
            state.cache.mark_failed_attempt(&rooftop_id, &cache_endpoint, state.rate_limit, 60).await;
            if let Some((entry, age)) = state.cache.get(&rooftop_id, &cache_endpoint).await {
                tracing::info!("{}/{}: serving stale after upstream error", rooftop_id, endpoint);
                return cached_response(&entry.body, &entry.content_type, "STALE", age);
            }
            (status, body).into_response()
        }
        Err(e) => {
            tracing::error!("{}/{}: upstream fetch failed: {}", rooftop_id, endpoint, e);
            state.cache.mark_failed_attempt(&rooftop_id, &cache_endpoint, state.rate_limit, 60).await;
            if let Some((entry, age)) = state.cache.get(&rooftop_id, &cache_endpoint).await {
                tracing::info!("{}/{}: serving stale after fetch error", rooftop_id, endpoint);
                return cached_response(&entry.body, &entry.content_type, "STALE", age);
            }
            (StatusCode::BAD_GATEWAY, format!("Upstream fetch failed: {e}")).into_response()
        }
    }
}

fn extract_rate_limit_headers(headers: &reqwest::header::HeaderMap) -> String {
    let mut parts = Vec::new();
    for key in ["x-rate-limit", "x-rate-limit-remaining", "x-rate-limit-reset", "retry-after"] {
        if let Some(v) = headers.get(key).and_then(|v| v.to_str().ok()) {
            parts.push(format!("{}: {}", key, v));
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" [{}]", parts.join(", "))
    }
}

fn cached_response(body: &str, content_type: &str, cache_status: &str, age: i64) -> Response {
    (
        StatusCode::OK,
        [
            ("Content-Type", content_type.to_string()),
            ("X-Cache", cache_status.to_string()),
            ("X-Cache-Age", age.to_string()),
        ],
        body.to_string(),
    )
        .into_response()
}
