use http::Response;
use http_body::Body;
use http_body_util::Empty;

use super::{Http1, HttpProtocol};

#[derive(Clone)]
pub(super) struct ZerocopyResponse {
    pub(super) handle: super::RawHandle,
}

unsafe impl Send for ZerocopyResponse {}
unsafe impl Sync for ZerocopyResponse {}

/// Installs a zero-copy hint on an HTTP response, directing the connection
/// handler to use emulated sendfile (Linux only) to transmit the body.
///
/// # Parameters
///
/// - `response` – the response whose extensions will receive the hint.
/// - `handle` – the raw file descriptor of the file to send. The caller is
///   responsible for ensuring the file descriptor remains valid and open for
///   the entire duration of the response write.
///
/// # Safety
///
/// The caller must guarantee that `handle` is a valid, open file descriptor
/// that will not be closed before the response body has been fully sent.
pub unsafe fn install_zerocopy(response: &mut http::Response<impl Body>, handle: super::RawHandle) {
    response
        .extensions_mut()
        .insert(ZerocopyResponse { handle });
}

/// An HTTP/1.x connection handler that uses emulated sendfile for zero-copy
/// response body transmission on Linux.
///
/// Obtain an instance via [`Http1::zerocopy`]. When a response has a
/// `ZerocopyResponse` extension installed (see [`install_zerocopy`]), the
/// handler will use emulated sendfile (utilizing the Linux `splice(2)` syscall
/// and Unix pipes) to stream the file from the kernel page cache
/// to the socket, bypassing user-space copies.
///
/// For responses without that extension the behaviour is identical to the
/// regular [`Http1`] handler.
///
/// Only available on Linux (`target_os = "linux"`).
#[cfg(all(target_os = "linux", feature = "h1-zerocopy"))]
pub struct Http1Zerocopy<Io> {
    pub(super) inner: Http1<Io>,
}

#[cfg(all(target_os = "linux", feature = "h1-zerocopy"))]
impl<Io> HttpProtocol for Http1Zerocopy<Io>
where
    for<'a> Io: tokio::io::AsyncRead
        + tokio::io::AsyncWrite
        + vibeio::io::AsInnerRawHandle<'a>
        + Unpin
        + 'static,
{
    fn handle_with_error_fn<F, Fut, ResB, ResBE, ResE, EF, EFut, EResB, EResBE, EResE>(
        self,
        request_fn: F,
        error_fn: EF,
    ) -> impl std::future::Future<Output = Result<(), std::io::Error>>
    where
        F: Fn(http::Request<super::Incoming>) -> Fut,
        Fut: std::future::Future<Output = Result<http::Response<ResB>, ResE>>,
        ResB: Body<Data = bytes::Bytes, Error = ResBE> + Unpin,
        ResE: std::error::Error,
        ResBE: std::error::Error,
        EF: FnOnce(bool) -> EFut,
        EFut: std::future::Future<Output = Result<http::Response<EResB>, EResE>>,
        EResB: Body<Data = bytes::Bytes, Error = EResBE> + Unpin,
        EResE: std::error::Error,
        EResBE: std::error::Error,
    {
        self.inner.handle_with_error_fn_and_zerocopy(
            request_fn,
            error_fn,
            Some(move |fd, io, len| async move {
                use std::os::fd::BorrowedFd;

                let fd = unsafe { BorrowedFd::borrow_raw(fd) };
                let _ = vibeio::io::sendfile_exact(&fd, io, len).await?;
                Ok(())
            }),
        )
    }

    fn handle<F, Fut, ResB, ResBE, ResE>(
        self,
        request_fn: F,
    ) -> impl std::future::Future<Output = Result<(), std::io::Error>>
    where
        F: Fn(http::Request<super::Incoming>) -> Fut,
        Fut: std::future::Future<Output = Result<http::Response<ResB>, ResE>>,
        ResB: Body<Data = bytes::Bytes, Error = ResBE> + Unpin,
        ResE: std::error::Error,
        ResBE: std::error::Error,
    {
        self.handle_with_error_fn(request_fn, |is_timeout| async move {
            let mut response = Response::builder();
            if is_timeout {
                response = response.status(http::StatusCode::REQUEST_TIMEOUT);
            } else {
                response = response.status(http::StatusCode::BAD_REQUEST);
            }
            response.body(Empty::new())
        })
    }
}
