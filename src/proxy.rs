use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::AppState;

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
        let qs: Vec<String> = params.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
        format!("{}?{}", endpoint, qs.join("&"))
    };

    // Cache-Control: no-cache bypasses TTL (but not rate limit)
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

    // Cache is stale or missing — check rate limit
    if !state.cache.can_fetch(&rooftop_id, &cache_endpoint, state.rate_limit).await {
        // Rate limited — serve stale if available
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

    // Fetch upstream
    state.cache.mark_attempt(&rooftop_id, &cache_endpoint).await;
    let upstream_url = format!(
        "{}/rooftop_sites/{}/{}",
        state.upstream_url, rooftop_id, endpoint
    );

    // Forward the client's Authorization header and query params
    let mut req = state.client.get(&upstream_url).header("Accept", "application/json");
    if !params.is_empty() {
        req = req.query(&params);
    }
    if let Some(auth) = headers.get("Authorization") {
        req = req.header("Authorization", auth);
    }

    tracing::info!("{}/{}: fetching upstream", rooftop_id, endpoint);

    let result = req.send().await;

    match result {
        Ok(response) => {
            let status = response.status();
            let content_type = response
                .headers()
                .get("Content-Type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/json")
                .to_string();

            if status == StatusCode::TOO_MANY_REQUESTS {
                // Upstream rate limited us
                tracing::warn!("{}/{}: upstream returned 429", rooftop_id, endpoint);
                if let Some((entry, age)) = state.cache.get(&rooftop_id, &cache_endpoint).await {
                    return cached_response(&entry.body, &entry.content_type, "STALE", age);
                }
                return (StatusCode::TOO_MANY_REQUESTS, "Upstream rate limited").into_response();
            }

            if !status.is_success() {
                // Upstream error — serve stale if available
                let body = response.text().await.unwrap_or_default();
                tracing::error!("{}/{}: upstream error {} - {}", rooftop_id, endpoint, status, body);
                if let Some((entry, age)) = state.cache.get(&rooftop_id, &cache_endpoint).await {
                    tracing::info!("{}/{}: serving stale after upstream error", rooftop_id, endpoint);
                    return cached_response(&entry.body, &entry.content_type, "STALE", age);
                }
                return (status, body).into_response();
            }

            // Success — cache and return
            let body = match response.text().await {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!("{}/{}: failed to read upstream body: {}", rooftop_id, endpoint, e);
                    if let Some((entry, age)) = state.cache.get(&rooftop_id, &cache_endpoint).await {
                        return cached_response(&entry.body, &entry.content_type, "STALE", age);
                    }
                    return (StatusCode::BAD_GATEWAY, "Failed to read upstream response").into_response();
                }
            };

            state
                .cache
                .set(&rooftop_id, &cache_endpoint, body.clone(), content_type.clone())
                .await;
            tracing::info!("{}/{}: MISS (fetched {}B)", rooftop_id, endpoint, body.len());

            cached_response(&body, &content_type, "MISS", 0)
        }
        Err(e) => {
            // Network error — serve stale if available
            tracing::error!("{}/{}: upstream fetch failed: {}", rooftop_id, endpoint, e);
            if let Some((entry, age)) = state.cache.get(&rooftop_id, &cache_endpoint).await {
                tracing::info!("{}/{}: serving stale after fetch error", rooftop_id, endpoint);
                return cached_response(&entry.body, &entry.content_type, "STALE", age);
            }
            (StatusCode::BAD_GATEWAY, format!("Upstream fetch failed: {}", e)).into_response()
        }
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
