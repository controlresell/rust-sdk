# ControlResell Rust SDK

Rust client for the [ControlResell](https://controlresell.com) public API (async, on [reqwest](https://docs.rs/reqwest)
and [tokio](https://tokio.rs)).

## Install

```sh
cargo add controlresell-sdk
```

The package is `controlresell-sdk`, the library is `controlresell`.

```rust
use controlresell::{Client, ErrorKind};
use futures_util::TryStreamExt;

#[tokio::main]
async fn main() -> controlresell::Result<()> {
    let client = Client::builder().api_key("cr_live_...").build()?; // or set CONTROLRESELL_API_KEY

    let me = client.get_me().send().await?;
    println!("{} {}", me.id, me.username);

    // Every unsold item, page after page, with the same filters.
    let mut items = std::pin::pin!(client.list_all_items(|request| request.sold(false)));
    while let Some(item) = items.try_next().await? {
        println!("{} {}", item.id, item.name);
    }

    // Writes (API key with `items:write` / `conversations:write`). Each POST gets an Idempotency-Key, the same on every
    // retry; with_idempotency_key() chooses it, to retry a write yourself later without doing it twice.
    let item = client.create_item().body_map(|body| body.name("Jacket")).send().await?;
    let account = "<account id>".parse().unwrap();
    client
        .with_idempotency_key(format!("publish-{}", item.id))
        .publish_item()
        .id(item.id)
        .body_map(|body| body.account_ids(vec![account]))
        .send()
        .await?;

    // Errors are typed: the status, the API's code and the request id to give the support.
    if let Err(failure) = client.get_item().id(42).send().await.map_err(controlresell::Error::from) {
        if failure.kind() == Some(ErrorKind::NotFound) {
            let api = failure.api().unwrap();
            println!("{} {:?}", api.code, api.request_id);
        }
    }
    Ok(())
}
```

Every route of the API is a method of `Client` that returns a builder (`client.list_items().sold(false).send().await`),
with the filters as methods and the body as `body_map(|body| ...)` or `body(types::...)`. The models are in `types`.

Built in:

- **Retries**: 2 by default, exponential delay with jitter; a 429 waits for its `Retry-After`; 502, 503, 504, a
  409 `idempotency_in_progress` and network errors are retried, a 500 never is.
  `Client::builder().retry(RetryPolicy::none())` turns them off. The webhook test sends a real event: it is only
  retried on a 429 and on a connection that never opened.
- **Idempotency**: every `POST` carries an `Idempotency-Key` (UUID v4), the same on each retry of the call.
- **Pagination**: `list_all_...(|request| ...)` returns a `Stream` that reads the pages one after another, with the same
  filters; `list_...()` reads one page (`data`, `next_cursor`). `list_all_item_favorites(|r| r.id(42))` and
  `list_all_conversation_messages(|r| r.id(...))` take the resource identifier the same way.
- **Typed errors**: `controlresell::Error` converts from any call's error (`?` does it in a function that returns
  `controlresell::Result`). `Error::Api(ApiError)` has `status`, `code` (an open string), `message`, `params`,
  `request_id`, `retry_after` and `kind()` (`ErrorKind::NotFound`, `RateLimit`, `Server`...); `Error::Connection` when the
  API cannot be reached. An error page that is not the API's (a gateway's 502) is read as an `ApiError` with its status.
- **Clearing a field**: the typed `update_item` leaves a field alone or sets it. To clear one (`categoryId`, the
  prices...), `update_item_json(id, json!({ "categoryId": null }))` sends a JSON merge patch; so does
  `update_webhook_endpoint_json` for an endpoint's `description`.
- Amounts keep their decimal string (`price.amount`).

`ClientBuilder` also takes `base_url` (staging, a test environment), `timeout` and `http_client` (a `reqwest::Client`
with your proxy or TLS settings; the SDK still adds the credential, the retries and the keys).

## Generated and hand-written code

`src/generated.rs` is generated from the API's OpenAPI document by
[Progenitor](https://github.com/oxidecomputer/progenitor) (`scripts/generate.sh`); the rest of `src` is written by hand.
The document is OpenAPI 3.1.1 and Progenitor reads 3.0.x only, so `scripts/prepare_spec.py` rewrites it to 3.0 first
(nullable types, and the `Idempotency-Key` parameter, which the client sets itself). `tools/generate` runs Progenitor as a
library so the output does not depend on the machine's `rustfmt`.

```sh
scripts/generate.sh                       # from https://api.controlresell.com/api/public/openapi.json
scripts/generate.sh path/to/openapi.json  # or from a file
```

Licensed under the Apache License 2.0.
