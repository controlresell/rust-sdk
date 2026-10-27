use std::future::Future;

use futures_util::stream::{self, Stream, TryStreamExt};

use crate::Result;

/// Reads the pages one after another: `fetch` gets the cursor of the page to read (`None` for the first) and returns
/// its items with the cursor of the next one (`None` for the last); the stream yields the items.
pub(crate) fn paginate<'a, T, F, Fut>(fetch: F) -> impl Stream<Item = Result<T>> + 'a
where
    T: 'a,
    F: FnMut(Option<String>) -> Fut + 'a,
    Fut: Future<Output = Result<(Vec<T>, Option<String>)>> + 'a,
{
    stream::try_unfold((fetch, Some(None::<String>)), |(mut fetch, cursor)| async move {
        let Some(cursor) = cursor else { return Ok::<_, crate::Error>(None) };
        let (items, next) = fetch(cursor).await?;
        Ok(Some((stream::iter(items.into_iter().map(Ok)), (fetch, next.map(Some)))))
    })
    .try_flatten()
}
