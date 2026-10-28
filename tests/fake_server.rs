//! The SDK against a fake HTTP server: headers, retries, idempotency keys, typed errors, pagination.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use controlresell::{Client, Error, ErrorKind, RetryPolicy};
use futures_util::TryStreamExt;
use serde_json::{json, Value};
use wiremock::matchers::{header, header_exists, method, path, query_param};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const FAST: RetryPolicy =
    RetryPolicy { max_retries: 2, base_delay: Duration::from_millis(1), max_delay: Duration::from_millis(500) };

fn client(server: &MockServer) -> Client {
    Client::builder().api_key("cr_test_key").base_url(server.uri()).retry(FAST).build().unwrap()
}

fn item(id: i64) -> Value {
    json!({
        "id": id, "name": "Jacket", "photos": [], "colorIds": [], "labels": [], "sold": false, "materialIds": [],
        "enumValues": [], "attributes": [], "customFields": [],
        "createdAt": "2026-10-01T10:00:00Z", "updatedAt": "2026-10-01T10:00:00Z"
    })
}

fn error(code: &str) -> Value {
    json!({ "error": { "code": code, "message": format!("{code} happened"), "params": { "id": "42" }, "requestId": "req_1" } })
}

fn page(items: Vec<Value>, next: Option<&str>) -> Value {
    json!({ "data": items, "nextCursor": next })
}

/// Answers the first `failures` calls with `failure`, then `success`; counts the calls.
struct Flaky {
    calls: Arc<AtomicUsize>,
    failures: usize,
    failure: ResponseTemplate,
    success: ResponseTemplate,
}

impl Respond for Flaky {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        if self.calls.fetch_add(1, Ordering::SeqCst) < self.failures {
            self.failure.clone()
        } else {
            self.success.clone()
        }
    }
}

fn flaky(failures: usize, failure: ResponseTemplate, success: ResponseTemplate) -> (Flaky, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    (Flaky { calls: calls.clone(), failures, failure, success }, calls)
}

#[tokio::test]
async fn a_call_sends_the_key_and_the_user_agent() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/public/v1/me"))
        .and(header("authorization", "Bearer cr_test_key"))
        .and(header("user-agent", format!("controlresell-rust/{}", controlresell::VERSION).as_str()))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "id": 7, "username": "nathan", "currency": null })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let me = client(&server).get_me().send().await.unwrap().into_inner();
    assert_eq!((me.id, me.username.as_str(), me.currency), (7, "nathan", None));
}

#[tokio::test]
async fn an_error_is_typed_by_its_status_with_its_code_and_request_id() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/public/v1/items/42"))
        .respond_with(ResponseTemplate::new(404).set_body_json(error("item_not_found")))
        .mount(&server)
        .await;
    let failure: Error = client(&server).get_item().id(42).send().await.unwrap_err().into();
    assert!(failure.is_not_found());
    let api = failure.api().unwrap();
    assert_eq!((api.status, api.code.as_str(), api.request_id.as_deref()), (404, "item_not_found", Some("req_1")));
    assert_eq!(api.params["id"], "42");
    assert_eq!(api.kind(), ErrorKind::NotFound);
}

#[tokio::test]
async fn a_rate_limit_is_retried_after_retry_after_then_succeeds() {
    let server = MockServer::start().await;
    let (responder, calls) = flaky(
        1,
        ResponseTemplate::new(429).insert_header("Retry-After", "0.2").set_body_json(error("rate_limited")),
        ResponseTemplate::new(200).set_body_json(json!({ "id": 1, "username": "u" })),
    );
    Mock::given(path("/api/public/v1/me")).respond_with(responder).mount(&server).await;
    let started = std::time::Instant::now();
    client(&server).get_me().send().await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(started.elapsed() >= Duration::from_millis(200), "waited {:?}", started.elapsed());
}

#[tokio::test]
async fn a_rate_limit_after_the_retries_is_a_rate_limit_error_with_its_retry_after() {
    let server = MockServer::start().await;
    Mock::given(path("/api/public/v1/me"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0").set_body_json(error("rate_limited")))
        .expect(3)
        .mount(&server)
        .await;
    let failure: Error = client(&server).get_me().send().await.unwrap_err().into();
    assert!(failure.is_rate_limited());
    assert_eq!(failure.api().unwrap().retry_after, Some(Duration::ZERO));
}

#[tokio::test]
async fn a_post_keeps_the_same_idempotency_key_across_retries() {
    let server = MockServer::start().await;
    let (responder, calls) = flaky(
        2,
        ResponseTemplate::new(503).set_body_json(error("unavailable")),
        ResponseTemplate::new(201).set_body_json(item(5)),
    );
    Mock::given(method("POST")).and(path("/api/public/v1/items")).respond_with(responder).mount(&server).await;
    let created = client(&server).create_item().body_map(|body| body.name("Jacket")).send().await.unwrap();
    assert_eq!(created.id, 5);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let keys: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.headers.get("idempotency-key").unwrap().to_str().unwrap().to_owned())
        .collect();
    assert_eq!(keys.len(), 3);
    assert!(keys.iter().all(|k| *k == keys[0]) && keys[0].len() == 36, "{keys:?}");
}

#[tokio::test]
async fn publish_price_drop_and_republish_carry_scheduled_at() {
    let server = MockServer::start().await;
    let receipt = json!({
        "itemId": 7, "action": "publish", "accountIds": ["3f2c1c0e-5b7a-4a3e-9d1e-0a1b2c3d4e5f"],
        "scheduledAt": "2026-11-01T09:30:00Z"
    });
    for route in ["publish", "price-drop", "republish"] {
        Mock::given(method("POST"))
            .and(path(format!("/api/public/v1/items/7/{route}")))
            .respond_with(ResponseTemplate::new(202).set_body_json(receipt.clone()))
            .mount(&server)
            .await;
    }
    let client = client(&server);
    let at: chrono::DateTime<chrono::Utc> = "2026-11-01T09:30:00Z".parse().unwrap();
    let account: controlresell::types::ScheduledAction = serde_json::from_value(receipt.clone()).unwrap();
    assert_eq!(account.scheduled_at, at);
    let id = account.account_ids[0];

    let published =
        client.publish_item().id(7).body_map(|b| b.account_ids(vec![id]).scheduled_at(at)).send().await.unwrap();
    client.drop_item_price().id(7).body_map(|b| b.type_("percent").value("10").scheduled_at(at)).send().await.unwrap();
    client.republish_item().id(7).body_map(|b| b.scheduled_at(at)).send().await.unwrap();

    assert_eq!(published.scheduled_at, at);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    for request in requests {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["scheduledAt"], "2026-11-01T09:30:00Z", "{}", request.url);
    }
}

#[tokio::test]
async fn two_posts_get_two_keys_and_with_idempotency_key_chooses_it() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/public/v1/items"))
        .respond_with(ResponseTemplate::new(201).set_body_json(item(1)))
        .mount(&server)
        .await;
    let client = client(&server);
    client.create_item().body_map(|b| b.name("a")).send().await.unwrap();
    client.create_item().body_map(|b| b.name("b")).send().await.unwrap();
    client.with_idempotency_key("my-key").create_item().body_map(|b| b.name("c")).send().await.unwrap();
    let keys: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.headers.get("idempotency-key").unwrap().to_str().unwrap().to_owned())
        .collect();
    assert_ne!(keys[0], keys[1]);
    assert_eq!(keys[2], "my-key");
}

#[tokio::test]
async fn a_get_has_no_idempotency_key_and_a_500_of_a_post_is_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/public/v1/items/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(item(1)))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/public/v1/items"))
        .respond_with(ResponseTemplate::new(500).set_body_json(error("internal_error")))
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server);
    client.get_item().id(1).send().await.unwrap();
    let failure: Error = client.create_item().body_map(|b| b.name("a")).send().await.unwrap_err().into();
    assert_eq!(failure.kind(), Some(ErrorKind::Server));
    let requests = server.received_requests().await.unwrap();
    assert!(requests[0].headers.get("idempotency-key").is_none());
}

#[tokio::test]
async fn a_gateway_page_without_the_envelope_is_an_api_error_with_its_status() {
    let server = MockServer::start().await;
    Mock::given(path("/api/public/v1/me"))
        .respond_with(ResponseTemplate::new(502).set_body_string("<html>Bad Gateway</html>"))
        .expect(3)
        .mount(&server)
        .await;
    let failure: Error = client(&server).get_me().send().await.unwrap_err().into();
    let api = failure.api().unwrap();
    assert_eq!((api.status, api.code.as_str(), api.request_id.clone()), (502, "http_502", None));
    assert_eq!(api.kind(), ErrorKind::Server);
}

#[tokio::test]
async fn only_the_conflict_idempotency_in_progress_is_retried() {
    let server = MockServer::start().await;
    let (responder, calls) = flaky(
        1,
        ResponseTemplate::new(409).set_body_json(error("idempotency_in_progress")),
        ResponseTemplate::new(201).set_body_json(item(9)),
    );
    Mock::given(method("POST")).and(path("/api/public/v1/items")).respond_with(responder).mount(&server).await;
    client(&server).create_item().body_map(|b| b.name("a")).send().await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    let other = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(409).set_body_json(error("idempotency_key_reused")))
        .expect(1)
        .mount(&other)
        .await;
    let failure: Error = client(&other).create_item().body_map(|b| b.name("a")).send().await.unwrap_err().into();
    assert_eq!(failure.kind(), Some(ErrorKind::Conflict));
}

#[tokio::test]
async fn retries_can_be_turned_off() {
    let server = MockServer::start().await;
    Mock::given(path("/api/public/v1/me"))
        .respond_with(ResponseTemplate::new(503).set_body_json(error("unavailable")))
        .expect(1)
        .mount(&server)
        .await;
    let client = Client::builder().api_key("k").base_url(server.uri()).retry(RetryPolicy::none()).build().unwrap();
    assert!(client.get_me().send().await.is_err());
}

#[tokio::test]
async fn list_all_reads_every_page_with_the_same_filters() {
    let server = MockServer::start().await;
    Mock::given(path("/api/public/v1/items"))
        .and(query_param("sold", "false"))
        .and(query_param("limit", "2"))
        .and(wiremock::matchers::query_param_is_missing("cursor"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(vec![item(1), item(2)], Some("c2"))))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/api/public/v1/items"))
        .and(query_param("sold", "false"))
        .and(query_param("cursor", "c2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(vec![item(3)], None)))
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server);
    let ids: Vec<i64> = client
        .list_all_items(|request| request.sold(false).limit(2u32))
        .map_ok(|item| item.id)
        .try_collect()
        .await
        .unwrap();
    assert_eq!(ids, [1, 2, 3]);
}

#[tokio::test]
async fn list_all_stops_with_the_error_of_a_page() {
    let server = MockServer::start().await;
    Mock::given(path("/api/public/v1/items"))
        .and(query_param("cursor", "c2"))
        .respond_with(ResponseTemplate::new(403).set_body_json(error("missing_scope")))
        .mount(&server)
        .await;
    Mock::given(path("/api/public/v1/items"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(vec![item(1)], Some("c2"))))
        .mount(&server)
        .await;
    let client = client(&server);
    let result: Result<Vec<_>, Error> = client.list_all_items(|r| r).try_collect().await;
    assert_eq!(result.unwrap_err().kind(), Some(ErrorKind::PermissionDenied));
}

#[tokio::test]
async fn a_route_under_a_resource_and_the_conversation_filters_are_sent() {
    let server = MockServer::start().await;
    Mock::given(path("/api/public/v1/conversations"))
        .and(query_param("buyerId", "B1"))
        .and(query_param("itemId", "42"))
        .and(query_param("offerStatus", "PENDING"))
        .and(query_param("unread", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(vec![], None)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/api/public/v1/items/42/favorites"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(vec![], None)))
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server);
    client.list_conversations().buyer_id("B1").item_id(42).offer_status("PENDING").unread(true).send().await.unwrap();
    let favorites: Vec<_> = client.list_all_item_favorites(|r| r.id(42)).try_collect().await.unwrap();
    assert!(favorites.is_empty());
}

#[tokio::test]
async fn an_amount_keeps_its_decimal_string_and_a_missing_one_is_none() {
    let server = MockServer::start().await;
    let mut body = item(1);
    body["sellingPrice"] = json!({ "amount": "12.50", "currency": "EUR" });
    body["purchasePrice"] = Value::Null;
    Mock::given(path("/api/public/v1/items/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let item = client(&server).get_item().id(1).send().await.unwrap().into_inner();
    let price = item.selling_price.unwrap();
    assert_eq!((price.amount.as_str(), price.currency.as_str()), ("12.50", "EUR"));
    assert!(item.purchase_price.is_none());
}

#[tokio::test]
async fn delete_resolves_on_204_and_a_409_is_a_typed_error() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/api/public/v1/items/1"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/public/v1/items/2"))
        .respond_with(ResponseTemplate::new(409).set_body_json(error("item_published")))
        .mount(&server)
        .await;
    let client = client(&server);
    client.delete_item().id(1).send().await.unwrap();
    let failure: Error = client.delete_item().id(2).send().await.unwrap_err().into();
    assert_eq!(failure.kind(), Some(ErrorKind::Conflict));
}

#[tokio::test]
async fn publish_posts_the_accounts_with_one_key_across_retries() {
    let server = MockServer::start().await;
    let account = "7f9c24e8-3b1a-4d3e-9b1a-0a1b2c3d4e5f";
    let (responder, calls) = flaky(
        1,
        ResponseTemplate::new(504).set_body_json(error("timeout")),
        ResponseTemplate::new(202).set_body_json(json!({
            "itemId": 42, "action": "publish", "accountIds": [account], "scheduledAt": "2026-10-01T10:00:00Z"
        })),
    );
    Mock::given(method("POST"))
        .and(path("/api/public/v1/items/42/publish"))
        .and(header_exists("idempotency-key"))
        .respond_with(responder)
        .mount(&server)
        .await;
    let action = client(&server)
        .publish_item()
        .id(42)
        .body_map(|b| b.account_ids(vec![account.parse().unwrap()]))
        .send()
        .await
        .unwrap();
    assert_eq!(action.item_id, 42);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let requests = server.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["accountIds"][0], account);
    assert_eq!(requests[0].headers.get("idempotency-key"), requests[1].headers.get("idempotency-key"));
}

#[tokio::test]
async fn the_webhook_test_is_never_replayed_after_a_5xx_but_is_after_a_429() {
    let test_path = "/api/public/v1/webhook-endpoints/9b2e1c3a-0000-4000-8000-000000000001/test";
    let id = "9b2e1c3a-0000-4000-8000-000000000001";
    let result = json!({ "success": true, "statusCode": 200, "durationMs": 12, "error": null });

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(test_path))
        .respond_with(ResponseTemplate::new(503).set_body_json(error("unavailable")))
        .expect(1)
        .mount(&server)
        .await;
    let failure: Error =
        client(&server).test_webhook_endpoint().id(id.parse::<uuid::Uuid>().unwrap()).send().await.unwrap_err().into();
    assert_eq!(failure.kind(), Some(ErrorKind::Server));
    assert!(server.received_requests().await.unwrap()[0].headers.get("idempotency-key").is_none());

    let limited = MockServer::start().await;
    let (responder, calls) = flaky(
        1,
        ResponseTemplate::new(429).insert_header("Retry-After", "0").set_body_json(error("rate_limited")),
        ResponseTemplate::new(200).set_body_json(result),
    );
    Mock::given(method("POST")).and(path(test_path)).respond_with(responder).mount(&limited).await;
    let tested = client(&limited).test_webhook_endpoint().id(id.parse::<uuid::Uuid>().unwrap()).send().await.unwrap();
    assert!(tested.success);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_server_that_is_down_is_a_connection_error_after_the_retries() {
    // A port nobody listens on.
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let client =
        Client::builder().api_key("k").base_url(format!("http://127.0.0.1:{port}")).retry(FAST).build().unwrap();
    let failure: Error = client.get_me().send().await.unwrap_err().into();
    assert!(matches!(failure, Error::Connection(_)), "{failure:?}");
}

#[tokio::test]
async fn webhook_events_read_any_data_and_follow_the_cursor() {
    let server = MockServer::start().await;
    let event = |id: &str, data: Value| json!({ "id": id, "type": "item.sold", "createdAt": "2026-10-01T10:00:00Z", "apiVersion": "1", "sellerId": 7, "data": data });
    Mock::given(path("/api/public/v1/events"))
        .and(query_param("type", "item.sold"))
        .and(query_param("cursor", "n"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(page(
                vec![event("00000000-0000-4000-8000-0000000000e2", json!({ "any": [1, 2] }))],
                None,
            )),
        )
        .mount(&server)
        .await;
    Mock::given(path("/api/public/v1/events"))
        .and(query_param("type", "item.sold"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(
            vec![event("00000000-0000-4000-8000-0000000000e1", json!({ "itemId": 3 }))],
            Some("n"),
        )))
        .mount(&server)
        .await;
    let client = client(&server);
    let events: Vec<_> = client.list_all_webhook_events(|r| r.type_("item.sold")).try_collect().await.unwrap();
    assert_eq!(
        events.iter().map(|e| e.id.to_string()).collect::<Vec<_>>(),
        ["00000000-0000-4000-8000-0000000000e1", "00000000-0000-4000-8000-0000000000e2"]
    );
}

#[test]
fn without_a_key_the_builder_refuses_and_the_debug_output_hides_it() {
    // The variable is read only when no key is given; this test never sets it.
    if std::env::var_os(controlresell::API_KEY_VARIABLE).is_none() {
        assert!(matches!(Client::builder().build(), Err(Error::InvalidRequest(_))));
    }
    let client = Client::builder().api_key("cr_live_secret").build().unwrap();
    assert!(!format!("{client:?}").contains("cr_live_secret"));
}

#[tokio::test]
async fn a_merge_patch_can_send_null_and_is_retried_without_a_key() {
    let server = MockServer::start().await;
    let (responder, calls) = flaky(
        1,
        ResponseTemplate::new(503).set_body_json(error("unavailable")),
        ResponseTemplate::new(200).set_body_json(item(42)),
    );
    Mock::given(method("PATCH")).and(path("/api/public/v1/items/42")).respond_with(responder).mount(&server).await;
    let updated = client(&server).update_item_json(42, json!({ "name": "Jacket", "categoryId": null })).await.unwrap();
    assert_eq!(updated.id, 42);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let requests = server.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(sent, json!({ "name": "Jacket", "categoryId": null }));
    assert_eq!(requests[1].headers.get("authorization").unwrap(), "Bearer cr_test_key");
    assert!(requests[1].headers.get("idempotency-key").is_none());

    let refusing = MockServer::start().await;
    Mock::given(method("PATCH"))
        .respond_with(ResponseTemplate::new(422).set_body_json(error("invalid_field")))
        .mount(&refusing)
        .await;
    let failure = client(&refusing).update_item_json(1, json!({ "state": 0 })).await.unwrap_err();
    assert_eq!(failure.kind(), Some(ErrorKind::UnprocessableEntity));
}
