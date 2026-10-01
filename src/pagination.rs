// Copied by wuapi-codegen from packages/sdk-codegen/templates/rust/src/pagination.rs. Do not edit here.

//! Cursor pagination.

use std::marker::PhantomData;

use futures_util::stream;
pub use futures_util::stream::{BoxStream, StreamExt, TryStreamExt};
use serde::de::DeserializeOwned;

use crate::error::Error;
use crate::http::{HttpClient, RequestParts};

/// One page of a list: its items, and the cursor of the next page.
pub trait CursorPage: DeserializeOwned + Send + 'static {
    /// What the list holds.
    type Item: Send + 'static;

    /// The cursor of the next page. `None` on the last page.
    fn next_cursor(&self) -> Option<&str>;

    /// The page's items.
    fn into_items(self) -> Vec<Self::Item>;
}

/// A list result. Stream it to walk every item across pages, or ask for a
/// single page.
///
/// ```ignore
/// use wuapi::pagination::TryStreamExt;
///
/// let mut messages = client.messages().list(params.clone()).stream();
/// while let Some(message) = messages.try_next().await? { /* ... */ }
///
/// let page = client.messages().list(params).page().await?;
/// ```
#[must_use = "a list does nothing until a page is requested or it is streamed"]
#[derive(Clone, Debug)]
pub struct Paginator<P> {
    http: HttpClient,
    parts: RequestParts,
    cursor_param: &'static str,
    cursor: Option<String>,
    page: PhantomData<fn() -> P>,
}

impl<P: CursorPage> Paginator<P> {
    /// A list over `parts` (which must not carry the cursor), starting at
    /// `cursor`. `cursor_param` is the query parameter the cursor travels in.
    pub fn new(
        http: HttpClient,
        parts: RequestParts,
        cursor_param: &'static str,
        cursor: Option<String>,
    ) -> Self {
        Self {
            http,
            parts,
            cursor_param,
            cursor,
            page: PhantomData,
        }
    }

    async fn fetch(&self, cursor: Option<&str>) -> Result<P, Error> {
        let parts = self.parts.clone().query_opt(self.cursor_param, cursor);
        self.http.request::<P>(parts).send().await
    }

    /// Fetches one page: the first, or the one at the cursor the list was
    /// created with.
    ///
    /// # Errors
    ///
    /// [`Error`] when the request fails.
    pub async fn page(&self) -> Result<P, Error> {
        self.fetch(self.cursor.as_deref()).await
    }

    /// Fetches the page at `cursor` (a previous page's next cursor).
    ///
    /// # Errors
    ///
    /// [`Error`] when the request fails.
    pub async fn page_at(&self, cursor: &str) -> Result<P, Error> {
        self.fetch(Some(cursor)).await
    }

    /// Every page, in order. The stream ends after the last page or the
    /// first error.
    pub fn pages(self) -> BoxStream<'static, Result<P, Error>> {
        enum State {
            At(Option<String>),
            Done,
        }
        let start = State::At(self.cursor.clone());
        stream::try_unfold((self, start), |(list, state)| async move {
            let State::At(cursor) = state else {
                return Ok(None);
            };
            let page = list.fetch(cursor.as_deref()).await?;
            let next = match page.next_cursor() {
                Some(next) if !next.is_empty() => State::At(Some(next.to_owned())),
                _ => State::Done,
            };
            Ok(Some((page, (list, next))))
        })
        .boxed()
    }

    /// Every item across pages, in order. The stream ends after the last
    /// item or the first error.
    pub fn stream(self) -> BoxStream<'static, Result<P::Item, Error>> {
        self.pages()
            .map_ok(|page| stream::iter(page.into_items().into_iter().map(Ok)))
            .try_flatten()
            .boxed()
    }

    /// Collects every item across pages.
    ///
    /// # Errors
    ///
    /// [`Error`] when a page fails; the items read so far are dropped.
    pub async fn to_vec(self) -> Result<Vec<P::Item>, Error> {
        self.stream().try_collect().await
    }

    /// Collects up to `max` items, fetching no more pages than needed.
    ///
    /// # Errors
    ///
    /// [`Error`] when a page fails; the items read so far are dropped.
    pub async fn to_vec_max(self, max: usize) -> Result<Vec<P::Item>, Error> {
        self.stream().take(max).try_collect().await
    }
}
