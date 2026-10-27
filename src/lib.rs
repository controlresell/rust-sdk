//! Rust SDK for the [ControlResell](https://controlresell.com) public API.
//!
//! ```no_run
//! use controlresell::Client;
//! use futures_util::TryStreamExt;
//!
//! # async fn run() -> controlresell::Result<()> {
//! let client = Client::builder().api_key("cr_live_...").build()?; // or set CONTROLRESELL_API_KEY
//!
//! let me = client.get_me().send().await?;
//! println!("{}", me.id);
//!
//! // Every unsold item, page after page.
//! let items: Vec<_> = client.list_all_items(|request| request.sold(false)).try_collect().await?;
//! # Ok(()) }
//! ```
//!
//! The client in `src/generated.rs` is generated from the API's OpenAPI document by
//! [Progenitor](https://github.com/oxidecomputer/progenitor) (`scripts/generate.sh`); the rest of the crate is
//! written by hand: [`Client::builder`], retries ([`RetryPolicy`]), an `Idempotency-Key` on every `POST`, typed errors
//! ([`Error`], [`ApiError`]) and the `list_all_*` helpers that follow the cursor.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod client;
mod error;
#[allow(
    clippy::all,
    clippy::pedantic,
    missing_docs,
    unused_imports,
    unused_variables,
    unreachable_pub,
    unreachable_patterns,
    rustdoc::broken_intra_doc_links,
    rustdoc::bare_urls,
    rustdoc::invalid_html_tags,
    rustdoc::invalid_rust_codeblocks
)]
#[rustfmt::skip]
mod generated;
mod pagination;
mod retry;

pub use client::{ClientBuilder, DEFAULT_BASE_URL, VERSION};
pub use error::{ApiError, Error, ErrorKind, Result};
pub use generated::{builder, types, Client};
pub use retry::{Inner, RetryPolicy};

/// The environment variable read when no API key is given to [`ClientBuilder::api_key`].
pub const API_KEY_VARIABLE: &str = "CONTROLRESELL_API_KEY";

/// The header that makes a `POST` safe to retry: the API replays the first answer to the same key.
pub const IDEMPOTENCY_KEY_HEADER: &str = "Idempotency-Key";
