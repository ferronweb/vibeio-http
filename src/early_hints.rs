use http::{HeaderMap, Request};
use http_body::Body;

type EarlyHintsResult = Result<(), std::io::Error>;
pub(super) type EarlyHintsMessage = (
    HeaderMap,
    futures_util::lock::Mutex<oneshot::Sender<EarlyHintsResult>>,
);

#[derive(Clone)]
pub(super) struct EarlyHints {
    inner: async_channel::Sender<EarlyHintsMessage>,
}

impl EarlyHints {
    pub(super) fn new(inner: async_channel::Sender<EarlyHintsMessage>) -> Self {
        Self { inner }
    }

    async fn send(&self, headers: HeaderMap) -> EarlyHintsResult {
        let (tx, rx) = oneshot::async_channel();
        self.inner
            .send((headers, futures_util::lock::Mutex::new(tx)))
            .await
            .map_err(std::io::Error::other)?;
        rx.await.map_err(std::io::Error::other)?
    }
}

/// Sends a `103 Early Hints` response for the given request.
///
/// Early hints allow the server to push `Link` (or other) headers to the
/// client before the final response is ready, enabling the browser to begin
/// preloading resources (stylesheets, scripts, fonts, etc.) while the handler
/// is still computing the response.
///
/// # Requirements
///
/// The connection must be HTTP/2 or HTTP/3, or if HTTP/1.1, the server must
/// have been created with
/// [`Http1Options::enable_early_hints`](crate::h1::Http1Options::enable_early_hints)
/// set to `true`. When that option is enabled the handler inserts an
/// `EarlyHints` token into each request's extensions. If the token is absent
/// (i.e. early hints were not enabled) this function returns an error.
///
/// # Errors
///
/// Returns `Err` if:
/// - Early hints are not enabled on the connection (`"early hints not supported"`).
/// - The connection handler has already shut down and can no longer accept
///   interim responses.
/// - Writing the `103` response to the underlying I/O stream fails.
///
/// # Example
///
/// ```rust,ignore
/// use http::HeaderMap;
///
/// let mut link_headers = HeaderMap::new();
/// link_headers.insert(
///     http::header::LINK,
///     "</style.css>; rel=preload; as=style".parse().unwrap(),
/// );
/// send_early_hints(&mut request, link_headers).await?;
/// ```
pub async fn send_early_hints(
    req: &mut Request<impl Body>,
    headers: HeaderMap,
) -> Result<(), std::io::Error> {
    req.extensions()
        .get::<EarlyHints>()
        .ok_or_else(|| std::io::Error::other("early hints not supported"))?
        .send(headers)
        .await
}
