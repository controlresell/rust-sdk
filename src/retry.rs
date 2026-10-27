use std::time::Duration;

use progenitor_client::{ClientHooks, OperationInfo};
use reqwest::header::{HeaderName, HeaderValue, AUTHORIZATION, RETRY_AFTER, USER_AGENT};
use reqwest::{Method, Request, Response, StatusCode};

use crate::{Client, VERSION};

/// How a call is retried.
///
/// A rate limit (429, after its `Retry-After`), a gateway or availability error (502, 503, 504), a 409
/// `idempotency_in_progress` and a network error are retried; a 500 never is. Every `POST` carries an
/// `Idempotency-Key` that stays the same across its retries, so a retried write is done once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// The number of retries after the first attempt; 0 turns them off.
    pub max_retries: u32,
    /// The first delay, doubled at each retry, with a random part; a `Retry-After` takes precedence.
    pub base_delay: Duration,
    /// The longest delay between two attempts.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    /// Two retries, 500 ms doubled each time, 30 s at most.
    fn default() -> Self {
        Self { max_retries: 2, base_delay: Duration::from_millis(500), max_delay: Duration::from_secs(30) }
    }
}

impl RetryPolicy {
    /// No retry: one attempt per call.
    pub fn none() -> Self {
        Self { max_retries: 0, ..Self::default() }
    }

    fn backoff(&self, attempt: u32) -> Duration {
        let base = self.base_delay.as_secs_f64();
        let mut delay = base * 2f64.powi(attempt.min(30) as i32);
        if base > 0.0 {
            delay += random_unit() * base / 2.0;
        }
        Duration::from_secs_f64(delay.min(self.max_delay.as_secs_f64()))
    }
}

fn random_unit() -> f64 {
    (uuid::Uuid::new_v4().as_u128() >> 64) as f64 / u64::MAX as f64
}

/// What the generated client carries next to the HTTP client: the credential and the retry policy. Built by
/// [`ClientBuilder`](crate::ClientBuilder); only named here because the generated `Client::new` takes it.
#[derive(Clone)]
pub struct Inner {
    pub(crate) api_key: String,
    pub(crate) retry: RetryPolicy,
    pub(crate) idempotency_key: Option<String>,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner").field("api_key", &"<redacted>").field("retry", &self.retry).finish_non_exhaustive()
    }
}

const IN_PROGRESS: &str = "idempotency_in_progress";

impl ClientHooks<Inner> for Client {
    async fn exec(&self, mut request: Request, info: &OperationInfo) -> reqwest::Result<Response> {
        let inner = &self.inner;
        let headers = request.headers_mut();
        if let Ok(mut value) = HeaderValue::from_str(&format!("Bearer {}", inner.api_key)) {
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
        if !headers.contains_key(USER_AGENT) {
            headers.insert(USER_AGENT, HeaderValue::from_str(&format!("controlresell-rust/{VERSION}")).expect("ascii"));
        }
        // The webhook test sends a real event and the API ignores its key: no key, and only a rate limit or a
        // connection that never opened is replayed.
        let replayable = info.operation_id != "test_webhook_endpoint";
        if *request.method() == Method::POST && replayable {
            let key = inner.idempotency_key.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            if let Ok(value) = HeaderValue::from_str(&key) {
                request.headers_mut().insert(HeaderName::from_static("idempotency-key"), value);
            }
        }

        let policy = inner.retry;
        let mut attempt = 0;
        loop {
            let last = attempt >= policy.max_retries;
            let Some(next) = (!last).then(|| request.try_clone()).flatten() else {
                return normalize(self.client.execute(request).await).await;
            };
            let delay = match self.client.execute(request).await {
                Err(error) => {
                    let retry = if replayable { !error.is_builder() && !error.is_decode() } else { error.is_connect() };
                    if !retry {
                        return Err(error);
                    }
                    policy.backoff(attempt)
                }
                Ok(response) => {
                    let status = response.status();
                    if !replayable && status != StatusCode::TOO_MANY_REQUESTS {
                        return normalize(Ok(response)).await;
                    }
                    let (response, retry) = should_retry(response).await;
                    if !retry {
                        return normalize(Ok(response)).await;
                    }
                    match retry_after(&response) {
                        Some(after) => after.min(policy.max_delay),
                        None => policy.backoff(attempt),
                    }
                }
            };
            tokio::time::sleep(delay).await;
            request = next;
            attempt += 1;
        }
    }
}

/// Whether to retry an answer. A 409 is read to tell `idempotency_in_progress` from the others, and rebuilt.
async fn should_retry(response: Response) -> (Response, bool) {
    match response.status() {
        StatusCode::TOO_MANY_REQUESTS
        | StatusCode::BAD_GATEWAY
        | StatusCode::SERVICE_UNAVAILABLE
        | StatusCode::GATEWAY_TIMEOUT => (response, true),
        StatusCode::CONFLICT => {
            let (response, body) = read(response).await;
            let code = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.pointer("/error/code").and_then(|c| c.as_str().map(str::to_owned)));
            (response, code.as_deref() == Some(IN_PROGRESS))
        }
        _ => (response, false),
    }
}

fn retry_after(response: &Response) -> Option<Duration> {
    let seconds: f64 = response.headers().get(RETRY_AFTER)?.to_str().ok()?.trim().parse().ok()?;
    (seconds >= 0.0 && seconds.is_finite()).then(|| Duration::from_secs_f64(seconds))
}

/// Reads a body and gives back an equivalent response, so it can still be handed to the generated client.
async fn read(response: Response) -> (Response, bytes::Bytes) {
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.bytes().await.unwrap_or_default();
    let mut rebuilt = http::Response::new(body.clone());
    *rebuilt.status_mut() = status;
    *rebuilt.headers_mut() = headers;
    (Response::from(rebuilt), body)
}

/// Makes every error answer the API's envelope `{"error": {code, message, params, requestId}}`: a gateway's 502 page,
/// or an envelope missing a field, becomes one (`http_502`, `HTTP 502`) so the generated client reads it as an
/// error of the API with its status.
async fn normalize(result: reqwest::Result<Response>) -> reqwest::Result<Response> {
    let response = result?;
    let status = response.status();
    if !(status.is_client_error() || status.is_server_error()) {
        return Ok(response);
    }
    let (response, body) = read(response).await;
    let mut value: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    let error = value.get_mut("error").and_then(|e| e.as_object_mut());
    let complete = error.as_ref().is_some_and(|e| {
        e.get("code").is_some_and(|c| c.is_string())
            && e.get("message").is_some_and(|c| c.is_string())
            && e.get("params").is_some_and(|c| c.is_object())
            && e.get("requestId").is_some_and(|c| c.is_string())
    });
    if complete {
        return Ok(response);
    }
    let mut envelope = error.map(std::mem::take).unwrap_or_default();
    envelope.entry("code").or_insert_with(|| format!("http_{}", status.as_u16()).into());
    envelope.entry("message").or_insert_with(|| format!("HTTP {}", status.as_u16()).into());
    envelope.entry("params").or_insert_with(|| serde_json::json!({}));
    envelope.entry("requestId").or_insert_with(|| "".into());
    for key in ["code", "message", "requestId"] {
        if !envelope[key].is_string() {
            envelope.insert(key.into(), "".into());
        }
    }
    if !envelope["params"].is_object() {
        envelope.insert("params".into(), serde_json::json!({}));
    }
    let headers = response.headers().clone();
    let mut rebuilt = http::Response::new(bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({ "error": envelope })).expect("json"),
    ));
    *rebuilt.status_mut() = status;
    *rebuilt.headers_mut() = headers;
    rebuilt.headers_mut().insert(reqwest::header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    rebuilt.headers_mut().remove(reqwest::header::CONTENT_LENGTH);
    Ok(Response::from(rebuilt))
}
