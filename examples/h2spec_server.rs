use bytes::Bytes;
use http::{Response, StatusCode};
use http_body_util::Full;
use vibeio::net::TcpListener;
use vibeio::RuntimeBuilder;
use vibeio_http::{Http2, Http2Options, HttpProtocol};

// h2spec conformance server. Listen on 127.0.0.1:8080 and answer every
// request with a 200 response carrying a small non-empty body. Run via
// `scripts/h2spec.sh`, which starts this binary, waits for readiness, runs
// h2spec with `--strict`, and propagates its exit code.
fn main() -> std::io::Result<()> {
    let runtime = RuntimeBuilder::new().enable_timer(true).build()?;
    runtime.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:8080")?;
        loop {
            let (stream, _) = listener.accept().await?;
            stream.set_nodelay(true)?;
            let conn = Http2::new(stream.into_poll()?, Http2Options::default());
            vibeio::spawn(async move {
                let _ = conn
                    .handle(|_request| async move {
                        let response = Response::builder()
                            .status(StatusCode::OK)
                            .body(Full::new(Bytes::from_static(b"Hello World")))
                            .expect("valid response");
                        Ok::<_, std::convert::Infallible>(response)
                    })
                    .await;
            });
        }
    })
}
