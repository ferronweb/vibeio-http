//! # vibeio-http
//!
//! High-performance HTTP/1.1, HTTP/2, and experimental HTTP/3 server implementation
//! for the [`vibeio`] async runtime.
//!
//! `vibeio-http` provides protocol-specific connection handlers behind a common
//! [`HttpProtocol`] trait. Handlers receive an `http::Request<`[`Incoming`]`>`
//! and return an `http::Response<B>` where `B` implements
//! [`http_body::Body`] with `bytes::Bytes` chunks.
//!
//! ## Feature highlights
//!
//! - **Zero-copy static file serving** - supports zero-copy response sending for static file serving on Linux.
//! - **100 Continue** - Supports automatically sending `100 Continue` before the final response.
//! - **103 Early Hints** - Supports sending `103 Early Hints` before the final response.
//!
//! ## Feature flags
//!
//! - `h1`: Enables HTTP/1.x support.
//! - `h2`: Enables HTTP/2 support.
//! - `h3`: Enables HTTP/3 support.
//! - `h1-zerocopy`: Enables Linux-only zero-copy response sending for HTTP/1.x.
//!
//! ## Early hints
//!
//! Use [`send_early_hints`] from a request handler to send `103 Early Hints`
//! before the final response.
//!
//! ## Example
//!
//! ```rust,no_run
//! # #[cfg(feature = "h1")]
//! # fn main() -> std::io::Result<()> {
//! use bytes::Bytes;
//! use http::Response;
//! use http_body_util::Full;
//! use vibeio::net::TcpListener;
//! use vibeio::RuntimeBuilder;
//! use vibeio_http::{Http1, Http1Options, HttpProtocol};
//!
//! let runtime = RuntimeBuilder::new().enable_timer(true).build()?;
//!
//! runtime.block_on(async {
//!     let listener = TcpListener::bind("127.0.0.1:8080")?;
//!     let (stream, _) = listener.accept().await?;
//!     stream.set_nodelay(true)?;
//!
//!     Http1::new(stream.into_poll()?, Http1Options::default())
//!         .handle(|_request| async move {
//!             let response = Response::new(Full::new(Bytes::from_static(b"Hello World")));
//!             Ok::<_, std::convert::Infallible>(response)
//!         })
//!         .await
//! })
//! # }
//! # #[cfg(not(feature = "h1"))]
//! # fn main() {}
//! ```
#![cfg_attr(docsrs, feature(doc_cfg))]

mod early_hints;
#[cfg(feature = "h1")]
mod h1;
#[cfg(feature = "h2")]
mod h2;
#[cfg(feature = "h3")]
mod h3;
mod incoming;

pub use early_hints::*;
#[cfg(feature = "h1")]
pub use h1::*;
#[cfg(feature = "h2")]
pub use h2::*;
#[cfg(feature = "h3")]
pub use h3::*;
pub use incoming::*;

use http::{Request, Response};
use http_body::Body;

/// A trait representing an HTTP protocol (for example, HTTP/1.1, HTTP/2, HTTP/3).
///
/// This trait provides a simple, type-erased interface for handling HTTP requests
/// across different protocol versions.
pub trait HttpProtocol: Sized {
    fn handle<F, Fut, ResB, ResBE, ResE>(
        self,
        request_fn: F,
    ) -> impl std::future::Future<Output = Result<(), std::io::Error>>
    where
        F: Fn(Request<Incoming>) -> Fut + 'static,
        Fut: std::future::Future<Output = Result<Response<ResB>, ResE>>,
        ResB: Body<Data = bytes::Bytes, Error = ResBE> + Unpin,
        ResE: std::error::Error,
        ResBE: std::error::Error;

    #[allow(unused_variables)]
    #[inline]
    fn handle_with_error_fn<F, Fut, ResB, ResBE, ResE, EF, EFut, EResB, EResBE, EResE>(
        self,
        request_fn: F,
        error_fn: EF,
    ) -> impl std::future::Future<Output = Result<(), std::io::Error>>
    where
        F: Fn(Request<Incoming>) -> Fut + 'static,
        Fut: std::future::Future<Output = Result<Response<ResB>, ResE>>,
        ResB: Body<Data = bytes::Bytes, Error = ResBE> + Unpin,
        ResE: std::error::Error,
        ResBE: std::error::Error,
        EF: FnOnce(bool) -> EFut,
        EFut: std::future::Future<Output = Result<Response<EResB>, EResE>>,
        EResB: Body<Data = bytes::Bytes, Error = EResBE> + Unpin,
        EResE: std::error::Error,
        EResBE: std::error::Error,
    {
        self.handle(request_fn)
    }
}
