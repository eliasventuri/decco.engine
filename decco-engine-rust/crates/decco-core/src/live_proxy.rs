use axum::{
    body::Body,
    extract::Query,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;
use tracing::error;
use url::Url;

#[derive(Debug, Deserialize)]
pub struct LiveProxyQuery {
    pub url: Option<String>,
    #[serde(rename = "ref")]
    pub referrer: Option<String>,
    pub ua: Option<String>,
    pub probe: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ForwardOpts {
    pub referrer: Option<String>,
    pub user_agent: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveProbeResponse {
    pub content_type: String,
    pub final_url: String,
    pub status: u16,
    pub first_bytes: String,
    pub sample_text: String,
}

fn is_allowed_live_proxy_protocol(scheme: &str) -> bool {
    scheme == "http" || scheme == "https"
}

fn build_engine_live_proxy_url(target: &str, host: &str, opts: &ForwardOpts) -> String {
    let base = format!("http://{}/live/proxy", host);
    if let Ok(mut proxied) = Url::parse(&base) {
        {
            let mut pairs = proxied.query_pairs_mut();
            pairs.append_pair("url", target);
            if let Some(ref r) = opts.referrer {
                if !r.is_empty() {
                    pairs.append_pair("ref", r);
                }
            }
            if let Some(ref ua) = opts.user_agent {
                if !ua.is_empty() {
                    pairs.append_pair("ua", ua);
                }
            }
        }
        proxied.to_string()
    } else {
        target.to_string()
    }
}

fn absolutize_live_proxy_line(line: &str, base_url: &str) -> String {
    if let Ok(base) = Url::parse(base_url) {
        if let Ok(joined) = base.join(line) {
            return joined.to_string();
        }
    }
    line.to_string()
}

pub fn rewrite_live_manifest(manifest: &str, base_url: &str, host: &str, opts: &ForwardOpts) -> String {
    let uri_re = Regex::new(r#"URI="([^"]+)""#).unwrap();
    manifest
        .split('\n')
        .map(|raw_line| {
            let line = raw_line.trim();
            if line.is_empty() {
                return raw_line.to_string();
            }
            if line.starts_with('#') {
                if raw_line.contains("URI=\"") {
                    return uri_re
                        .replace_all(raw_line, |caps: &regex::Captures| {
                            let uri = &caps[1];
                            if uri.is_empty() || uri.starts_with("data:") || uri.starts_with("blob:")
                            {
                                format!("URI=\"{}\"", uri)
                            } else {
                                let absolute = absolutize_live_proxy_line(uri, base_url);
                                format!(
                                    "URI=\"{}\"",
                                    build_engine_live_proxy_url(&absolute, host, opts)
                                )
                            }
                        })
                        .to_string();
                }
                return raw_line.to_string();
            }

            let absolute = absolutize_live_proxy_line(line, base_url);
            build_engine_live_proxy_url(&absolute, host, opts)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn rewrite_live_dash_manifest(
    manifest: &str,
    base_url: &str,
    host: &str,
    opts: &ForwardOpts,
) -> String {
    let base_url_re = Regex::new(r"<(BaseURL)>([^<]+)</BaseURL>").unwrap();
    let attr_re = Regex::new(r#"(media|initialization|sourceURL|href)="([^"]+)""#).unwrap();

    let step1 = base_url_re.replace_all(manifest, |caps: &regex::Captures| {
        let tag = &caps[1];
        let val = &caps[2];
        if val.is_empty() || val.starts_with("data:") || val.starts_with("blob:") {
            caps[0].to_string()
        } else {
            let absolute = absolutize_live_proxy_line(val, base_url);
            format!(
                "<{tag}>{}</{tag}>",
                build_engine_live_proxy_url(&absolute, host, opts)
            )
        }
    });

    attr_re
        .replace_all(&step1, |caps: &regex::Captures| {
            let attr = &caps[1];
            let val = &caps[2];
            if val.is_empty() || val.starts_with("data:") || val.starts_with("blob:") {
                caps[0].to_string()
            } else {
                let absolute = absolutize_live_proxy_line(val, base_url);
                format!(
                    "{attr}=\"{}\"",
                    build_engine_live_proxy_url(&absolute, host, opts)
                )
            }
        })
        .to_string()
}

pub async fn handle_live_proxy(
    Query(query): Query<LiveProxyQuery>,
    headers: HeaderMap,
    primary_port: u16,
) -> Response {
    let target = match query.url {
        Some(ref u) if !u.trim().is_empty() => u.trim().to_string(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Missing url parameter" })),
            )
                .into_response()
        }
    };

    let parsed = match Url::parse(&target) {
        Ok(u) => u,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Invalid url parameter" })),
            )
                .into_response()
        }
    };

    if !is_allowed_live_proxy_protocol(parsed.scheme()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Unsupported protocol" })),
        )
            .into_response();
    }

    let should_probe = query.probe.as_deref() == Some("1");
    let host = headers
        .get("host")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("127.0.0.1:{}", primary_port));

    let mut forward_opts = ForwardOpts {
        referrer: query.referrer.clone().filter(|s| !s.is_empty()),
        user_agent: query.ua.clone().filter(|s| !s.is_empty()),
    };

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    };

    let ua = forward_opts
        .user_agent
        .clone()
        .or_else(|| {
            headers
                .get("user-agent")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| {
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36".to_string()
        });

    let accept = headers
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("*/*")
        .to_string();

    let range_header = headers
        .get("range")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let build_req = |method: reqwest::Method, ref_override: Option<&str>, range_ov: Option<&str>| {
        let mut req = client
            .request(method, parsed.as_str())
            .header("User-Agent", &ua)
            .header("Accept", &accept);

        if let Some(r) = ref_override {
            req = req.header("Referer", r);
            if let Ok(ref_url) = Url::parse(r) {
                let origin = ref_url.origin().ascii_serialization();
                if origin != "null" {
                    req = req.header("Origin", origin);
                }
            }
        }

        if let Some(rng) = range_ov {
            req = req.header("Range", rng);
        } else if !should_probe {
            if let Some(ref rng) = range_header {
                req = req.header("Range", rng);
            }
        }
        req
    };

    let initial_method = if should_probe {
        reqwest::Method::HEAD
    } else {
        reqwest::Method::GET
    };

    let mut upstream = match build_req(
        initial_method.clone(),
        forward_opts.referrer.as_deref(),
        None,
    )
    .send()
    .await
    {
        Ok(r) => r,
        Err(e) => {
            error!("[Live Proxy] Failed: {}", e);
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "Failed to proxy live stream", "details": e.to_string() })),
            )
                .into_response();
        }
    };

    // Retry with spoofed Referer/Origin on 401/403
    if (upstream.status() == reqwest::StatusCode::FORBIDDEN
        || upstream.status() == reqwest::StatusCode::UNAUTHORIZED)
        && forward_opts.referrer.is_none()
    {
        let origin_str = parsed.origin().ascii_serialization();
        let spoof_ref = format!("{}/", origin_str);
        if let Ok(retry_res) = build_req(initial_method, Some(&spoof_ref), None)
            .send()
            .await
        {
            if retry_res.status().is_success() {
                forward_opts.referrer = Some(spoof_ref);
            }
            upstream = retry_res;
        }
    }

    // Probe fallback from HEAD to GET Range: bytes=0-511 on 403/405
    if should_probe
        && (upstream.status() == reqwest::StatusCode::FORBIDDEN
            || upstream.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED)
    {
        if let Ok(retry_res) = build_req(
            reqwest::Method::GET,
            forward_opts.referrer.as_deref(),
            Some("bytes=0-511"),
        )
        .send()
        .await
        {
            upstream = retry_res;
        }
    }

    if !upstream.status().is_success() {
        let code =
            StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        return (
            code,
            Json(json!({ "error": format!("Upstream request failed with {}", upstream.status().as_u16()) })),
        )
            .into_response();
    }

    let content_type = upstream
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let effective_base_url = upstream.url().to_string();
    let effective_pathname = upstream.url().path().to_ascii_lowercase();

    if should_probe {
        let status_u16 = upstream.status().as_u16();
        let bytes = upstream.bytes().await.unwrap_or_default();
        let slice = &bytes[..bytes.len().min(512)];
        let sample_text = String::from_utf8_lossy(slice).to_string();
        let first_bytes = slice[..slice.len().min(16)]
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>();

        return (
            StatusCode::OK,
            [("Cache-Control", "no-store")],
            Json(LiveProbeResponse {
                content_type,
                final_url: effective_base_url,
                status: status_u16,
                first_bytes,
                sample_text,
            }),
        )
            .into_response();
    }

    let is_known_hls = effective_pathname.ends_with(".m3u8")
        || effective_pathname.ends_with(".m3u")
        || content_type.to_ascii_lowercase().contains("mpegurl");
    let is_known_dash = effective_pathname.ends_with(".mpd")
        || content_type
            .to_ascii_lowercase()
            .contains("application/dash+xml");

    if is_known_hls {
        let manifest = upstream.text().await.unwrap_or_default();
        let rewritten =
            rewrite_live_manifest(&manifest, &effective_base_url, &host, &forward_opts);
        return (
            StatusCode::OK,
            [
                ("Content-Type", "application/vnd.apple.mpegurl"),
                ("Cache-Control", "no-store"),
            ],
            rewritten,
        )
            .into_response();
    }

    if is_known_dash {
        let manifest = upstream.text().await.unwrap_or_default();
        let rewritten =
            rewrite_live_dash_manifest(&manifest, &effective_base_url, &host, &forward_opts);
        return (
            StatusCode::OK,
            [
                ("Content-Type", "application/dash+xml"),
                ("Cache-Control", "no-store"),
            ],
            rewritten,
        )
            .into_response();
    }

    let content_length = upstream
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);

    let might_be_text_manifest = content_type.starts_with("text/")
        || content_type.contains("xml")
        || (content_length > 0
            && content_length < 256 * 1024
            && !effective_pathname.ends_with(".ts")
            && !effective_pathname.ends_with(".m4s"));

    if might_be_text_manifest {
        return match upstream.bytes().await {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                let trimmed = text.trim_start();
                if trimmed.starts_with("#EXTM3U") {
                    let rewritten =
                        rewrite_live_manifest(&text, &effective_base_url, &host, &forward_opts);
                    return (
                        StatusCode::OK,
                        [
                            ("Content-Type", "application/vnd.apple.mpegurl"),
                            ("Cache-Control", "no-store"),
                        ],
                        rewritten,
                    )
                        .into_response();
                }
                if (trimmed.starts_with("<?xml") && trimmed.contains("<MPD"))
                    || trimmed.starts_with("<MPD")
                {
                    let rewritten = rewrite_live_dash_manifest(
                        &text,
                        &effective_base_url,
                        &host,
                        &forward_opts,
                    );
                    return (
                        StatusCode::OK,
                        [
                            ("Content-Type", "application/dash+xml"),
                            ("Cache-Control", "no-store"),
                        ],
                        rewritten,
                    )
                        .into_response();
                }

                let mut resp = Response::new(Body::from(bytes));
                *resp.status_mut() = StatusCode::OK;
                if !content_type.is_empty() {
                    if let Ok(val) = content_type.parse() {
                        resp.headers_mut().insert("Content-Type", val);
                    }
                }
                resp.headers_mut()
                    .insert("Cache-Control", "no-store".parse().unwrap());
                resp
            }
            Err(_) => (StatusCode::BAD_GATEWAY, "Failed to read upstream body").into_response(),
        };
    }

    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::OK);
    let mut response_builder = Response::builder()
        .status(status)
        .header("Cache-Control", "no-store");

    if !content_type.is_empty() {
        response_builder = response_builder.header("Content-Type", content_type);
    }

    for (key, value) in upstream.headers().iter() {
        let lower = key.as_str().to_ascii_lowercase();
        if lower == "content-type"
            || lower == "content-length"
            || lower == "content-encoding"
            || lower == "transfer-encoding"
        {
            continue;
        }
        response_builder = response_builder.header(key, value);
    }

    let stream = upstream.bytes_stream();
    let body = Body::from_stream(stream);
    response_builder.body(body).unwrap_or_else(|_| {
        (StatusCode::BAD_GATEWAY, "Failed to stream upstream body").into_response()
    })
}

