mod http;

use ::http::Response;
use bytes::Bytes;
use http_body_util::Full;
use vibeio::RuntimeBuilder;
use vibeio::net::TcpListener;

use crate::http::{Http1, HttpProtocol};

fn main() -> std::io::Result<()> {
    // 1. Build the runtime
    let runtime = RuntimeBuilder::new().enable_timer(true).build()?;

    // 2. Run the main future
    runtime.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:8080")?;
        println!("Listening on 127.0.0.1:8080");

        loop {
            let (stream, _) = listener.accept().await?;

            vibeio::spawn(async move {
                Http1::new(stream.into_poll()?)
                    .handle(|_request| async move {
                        let response = Response::new(Full::new(Bytes::from_static(b"Hello World")));
                        Ok::<_, std::convert::Infallible>(response)
                    })
                    .await
            });
        }
    })
}
