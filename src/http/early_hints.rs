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
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        rx.await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?
    }
}

pub async fn send_early_hints(
    req: &mut Request<impl Body>,
    headers: HeaderMap,
) -> Result<(), std::io::Error> {
    req.extensions()
        .get::<EarlyHints>()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "early hints not supported"))?
        .send(headers)
        .await
}
