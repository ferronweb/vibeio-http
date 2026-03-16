mod h1;
mod incoming;

pub use h1::*;
pub use incoming::*;

use http::{Request, Response};
use http_body::Body;

pub trait HttpProtocol {
    fn handle<F, Fut, ResB, ResBE, ResE>(
        self,
        request_fn: F,
    ) -> impl std::future::Future<Output = Result<(), std::io::Error>>
    where
        F: FnMut(Request<Incoming>) -> Fut,
        Fut: std::future::Future<Output = Result<Response<ResB>, ResE>>,
        ResB: Body<Data = bytes::Bytes, Error = ResBE> + Unpin,
        ResE: std::error::Error,
        ResBE: std::error::Error;
}
