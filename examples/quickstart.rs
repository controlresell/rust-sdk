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
    // publish_item, drop_item_price and republish_item take an optional scheduled_at (a chrono DateTime<Utc>, at most 30 days
    // ahead; add the chrono crate to build one): the action starts then instead of now (tomorrow here), the 202 receipt carries the date.
    let tomorrow = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 86_400;
    let at = chrono::DateTime::<chrono::Utc>::from_timestamp(tomorrow as i64, 0).unwrap();
    client
        .with_idempotency_key(format!("publish-{}", item.id))
        .publish_item()
        .id(item.id)
        .body_map(|body| body.account_ids(vec![account]).scheduled_at(at))
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
