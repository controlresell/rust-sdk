use std::time::Duration;

use futures_util::stream::Stream;
use progenitor_client::{ClientHooks, ClientInfo, Error as GeneratedError, OperationInfo, ResponseValue};
use serde::de::DeserializeOwned;

use crate::pagination::paginate;
use crate::{builder, types, Client, Error, Inner, Result, RetryPolicy, API_KEY_VARIABLE};
use types::ErrorResponse;

/// The version of this SDK.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The production address of the API.
pub const DEFAULT_BASE_URL: &str = "https://api.controlresell.com";

/// Builds a [`Client`].
///
/// ```no_run
/// let client = controlresell::Client::builder()
///     .api_key("cr_live_...")
///     .retry(controlresell::RetryPolicy::none())
///     .build()?;
/// # Ok::<(), controlresell::Error>(())
/// ```
#[derive(Debug, Default)]
pub struct ClientBuilder {
    api_key: Option<String>,
    base_url: Option<String>,
    retry: Option<RetryPolicy>,
    http: Option<reqwest::Client>,
    timeout: Option<Duration>,
}

impl ClientBuilder {
    /// The API key; absent, the `CONTROLRESELL_API_KEY` environment variable is read.
    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// The API's address (staging, a test environment).
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    /// How calls are retried (two retries by default).
    pub fn retry(mut self, policy: RetryPolicy) -> Self {
        self.retry = Some(policy);
        self
    }

    /// The HTTP client to call through (proxy, TLS, connection pool...). The retries, the credential and the
    /// `Idempotency-Key` are applied by the SDK, whatever the client.
    pub fn http_client(mut self, client: reqwest::Client) -> Self {
        self.http = Some(client);
        self
    }

    /// The timeout of a call, 60 seconds by default; ignored when [`http_client`](Self::http_client) is given.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Creates the client.
    pub fn build(self) -> Result<Client> {
        let api_key =
            self.api_key.or_else(|| std::env::var(API_KEY_VARIABLE).ok()).filter(|key| !key.is_empty()).ok_or_else(
                || Error::InvalidRequest(format!("no API key: use ClientBuilder::api_key or set {API_KEY_VARIABLE}")),
            )?;
        let http = match self.http {
            Some(client) => client,
            None => reqwest::Client::builder()
                .timeout(self.timeout.unwrap_or(Duration::from_secs(60)))
                .build()
                .map_err(Error::Connection)?,
        };
        let inner = Inner { api_key, retry: self.retry.unwrap_or_default(), idempotency_key: None };
        let base_url = self.base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
        Ok(Client::new_with_client(base_url.trim_end_matches('/'), http, inner))
    }
}

impl Client {
    /// A builder of clients.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    /// A client for the API key of the `CONTROLRESELL_API_KEY` environment variable, with the defaults.
    pub fn from_env() -> Result<Client> {
        ClientBuilder::default().build()
    }

    /// A copy of this client whose `POST`s all carry the given `Idempotency-Key`, to retry a write yourself later
    /// without doing it twice: the API replays the first answer to the same key.
    ///
    /// ```no_run
    /// # async fn run(client: controlresell::Client, item_id: i64) -> controlresell::Result<()> {
    /// client.with_idempotency_key(format!("publish-{item_id}")).publish_item().id(item_id).send().await?;
    /// # Ok(()) }
    /// ```
    pub fn with_idempotency_key(&self, key: impl Into<String>) -> Client {
        let mut client = self.clone();
        client.inner.idempotency_key = Some(key.into());
        client
    }
}

impl Client {
    /// Changes an item with a JSON merge patch, where `null` clears a field: `{"categoryId": null}`.
    ///
    /// The typed [`update_item`](Client::update_item) leaves a field alone or sets it; it cannot send `null`, which
    /// is how the API clears `categoryId`, `sizeId`, `state`, the prices and the others. A field left out keeps its
    /// value.
    ///
    /// ```no_run
    /// # async fn run(client: controlresell::Client) -> controlresell::Result<()> {
    /// let item = client.update_item_json(42, serde_json::json!({ "name": "Jacket", "brandId": null })).await?;
    /// # Ok(()) }
    /// ```
    pub async fn update_item_json(&self, id: i64, patch: serde_json::Value) -> Result<types::Item> {
        self.patch_json(&format!("/api/public/v1/items/{id}"), "update_item", &patch).await
    }

    /// Changes a webhook endpoint with a JSON merge patch, where `null` clears a field: `{"description": null}`.
    ///
    /// The typed [`update_webhook_endpoint`](Client::update_webhook_endpoint) cannot send `null`.
    pub async fn update_webhook_endpoint_json(
        &self,
        id: uuid::Uuid,
        patch: serde_json::Value,
    ) -> Result<types::WebhookEndpoint> {
        self.patch_json(&format!("/api/public/v1/webhook-endpoints/{id}"), "update_webhook_endpoint", &patch).await
    }

    async fn patch_json<T: DeserializeOwned>(
        &self,
        path: &str,
        operation_id: &'static str,
        body: &serde_json::Value,
    ) -> Result<T> {
        let request = self
            .client()
            .patch(format!("{}{path}", self.baseurl()))
            .json(body)
            .build()
            .map_err(|e| Error::InvalidRequest(e.to_string()))?;
        let response =
            ClientHooks::exec(self, request, &OperationInfo { operation_id }).await.map_err(Error::Connection)?;
        if response.status().is_success() {
            Ok(ResponseValue::<T>::from_response::<ErrorResponse>(response).await?.into_inner())
        } else {
            match ResponseValue::<ErrorResponse>::from_response::<ErrorResponse>(response).await {
                Ok(rejected) => Err(GeneratedError::ErrorResponse(rejected).into()),
                Err(failure) => Err(failure.into()),
            }
        }
    }
}

macro_rules! list_all {
    ($(#[$doc:meta])* $name:ident, $list:ident, $request:ident, $item:ident) => {
        impl Client {
            $(#[$doc])*
            ///
            /// `configure` sets the filters (and the identifier, for a route under a resource); it is applied to each
            /// page, with the same filters. Dropping the stream stops reading pages.
            pub fn $name<'a, F>(&'a self, configure: F) -> impl Stream<Item = Result<types::$item>> + 'a
            where
                F: Fn(builder::$request<'a>) -> builder::$request<'a> + 'a,
            {
                paginate(move |cursor| {
                    let request = configure(self.$list());
                    let request = match cursor {
                        Some(cursor) => request.cursor(cursor),
                        None => request,
                    };
                    async move {
                        let page = request.send().await?.into_inner();
                        Ok((page.data, page.next_cursor))
                    }
                })
            }
        }
    };
}

list_all!(
    /// Every account of the seller, page after page.
    list_all_accounts, list_accounts, ListAccounts, Account
);
list_all!(
    /// Every item of the seller's stock, page after page.
    list_all_items, list_items, ListItems, Item
);
list_all!(
    /// Every buyer who favorited the listings of one item (`items:read`), page after page.
    list_all_item_favorites, list_item_favorites, ListItemFavorites, Favorite
);
list_all!(
    /// Every favorite of the seller's listings (`items:read`), page after page.
    list_all_favorites, list_favorites, ListFavorites, Favorite
);
list_all!(
    /// Every conversation, page after page.
    list_all_conversations, list_conversations, ListConversations, Conversation
);
list_all!(
    /// Every message of one conversation, page after page.
    list_all_conversation_messages, list_conversation_messages, ListConversationMessages, Message
);
list_all!(
    /// Every customer, page after page.
    list_all_customers, list_customers, ListCustomers, Customer
);
list_all!(
    /// Every order, page after page.
    list_all_orders, list_orders, ListOrders, Order
);
list_all!(
    /// Every label of the orders to ship, page after page.
    list_all_labels_to_ship, list_labels_to_ship, ListLabelsToShip, OrderLabel
);
list_all!(
    /// Every catalog category, page after page.
    list_all_catalog_categories, list_catalog_categories, ListCatalogCategories, CatalogCategory
);
list_all!(
    /// Every catalog brand (the identifiers an item is written with), page after page.
    list_all_catalog_brands, list_catalog_brands, ListCatalogBrands, CatalogBrand
);
list_all!(
    /// Every catalog size, page after page.
    list_all_catalog_sizes, list_catalog_sizes, ListCatalogSizes, CatalogSize
);
list_all!(
    /// Every catalog color, page after page.
    list_all_catalog_colors, list_catalog_colors, ListCatalogColors, CatalogValue
);
list_all!(
    /// Every catalog condition, page after page.
    list_all_catalog_conditions, list_catalog_conditions, ListCatalogConditions, CatalogValue
);
list_all!(
    /// Every catalog package size, page after page.
    list_all_catalog_package_sizes, list_catalog_package_sizes, ListCatalogPackageSizes, CatalogValue
);
list_all!(
    /// Every catalog material, page after page.
    list_all_catalog_materials, list_catalog_materials, ListCatalogMaterials, CatalogValue
);
list_all!(
    /// Every delivery of one webhook endpoint over the last 7 days, page after page.
    list_all_webhook_deliveries, list_webhook_deliveries, ListWebhookDeliveries, WebhookDelivery
);
list_all!(
    /// Every event of the last 7 days, the oldest first: what a client that was offline catches up.
    list_all_webhook_events, list_webhook_events, ListWebhookEvents, WebhookEvent
);
