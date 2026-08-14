//! Native HTTP/3 server throughput / latency benchmark over a quinn loopback.
//!
//! The server under test is this crate's native `Http3` driver (one vibeio
//! runtime per connection). The client is the `h3` crate (0.0.8) over
//! `h3_quinn`, used purely as an RFC-compliant load generator. We measure
//! requests/second (criterion throughput) and p50/p99 latency for a fixed
//! GET request mix against a handler returning a small 200 body.
//!
//! Run with
//!
//! ```sh
//! cargo bench --features h3-quinn --bench h3_server
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use http::{Request, Response, StatusCode};
use http_body_util::Full;
use quinn::{ClientConfig, Endpoint, ServerConfig};
use std::convert::Infallible;
use tokio_util::sync::CancellationToken;
use vibeio::RuntimeBuilder;
use vibeio_http::{Http3, Http3Options, HttpProtocol, Incoming};

fn loopback() -> std::net::SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

/// A connected loopback pair plus the endpoints that keep both halves alive
/// (quinn tears the connections down if the endpoint is dropped).
async fn loopback_pair() -> (Endpoint, Endpoint, quinn::Connection, quinn::Connection) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_der: quinn::rustls::pki_types::CertificateDer<'static> = cert.cert.into();

    let server_config = ServerConfig::with_single_cert(
        vec![cert_der.clone()],
        quinn::rustls::pki_types::PrivateKeyDer::from(cert.signing_key),
    )
    .unwrap();
    let server_endpoint = Endpoint::server(server_config, loopback()).unwrap();
    let server_addr = server_endpoint.local_addr().unwrap();

    let mut roots = quinn::rustls::RootCertStore::empty();
    roots.add(cert_der).unwrap();
    let client_config = ClientConfig::with_root_certificates(Arc::new(roots)).unwrap();
    let mut client_endpoint = Endpoint::client(loopback()).unwrap();
    client_endpoint.set_default_client_config(client_config);

    let connecting = client_endpoint
        .connect(server_addr, "localhost")
        .expect("connect");
    let incoming = server_endpoint
        .accept()
        .await
        .expect("accept must eventually yield");
    let (client_conn, server_conn) = tokio::join!(
        async { connecting.await.expect("client handshake") },
        async { incoming.await.expect("server handshake") },
    );
    (server_endpoint, client_endpoint, client_conn, server_conn)
}

/// Runs the native server on a vibeio runtime in a dedicated thread.
fn spawn_native_server<F, Fut, ResB, ResBE, ResE>(
    server_conn: quinn::Connection,
    options: Http3Options,
    cancel: CancellationToken,
    handler: F,
) -> std::thread::JoinHandle<()>
where
    F: Fn(Request<Incoming>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<Response<ResB>, ResE>> + Send + 'static,
    ResB: http_body::Body<Data = Bytes, Error = ResBE> + Send + Unpin + 'static,
    ResBE: std::error::Error + Send + 'static,
    ResE: std::error::Error + Send + 'static,
{
    std::thread::spawn(move || {
        let rt = RuntimeBuilder::new()
            .enable_timer(true)
            .build()
            .expect("vibeio runtime");
        let _ = rt.block_on(async move {
            Http3::new(vibeio_http::quinn::Connection::new(server_conn), options)
                .graceful_shutdown_token(cancel)
                .handle(handler)
                .await
        });
    })
}

fn handler(
    _request: Request<Incoming>,
) -> impl std::future::Future<Output = Result<Response<Full<Bytes>>, Infallible>> {
    async move {
        Ok::<_, Infallible>(
            Response::builder()
                .status(StatusCode::OK)
                .body(Full::new(Bytes::from_static(b"Hello World")))
                .expect("valid response"),
        )
    }
}

async fn issue_one(send_request: &mut h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>) {
    let request = Request::get("https://localhost/bench").body(()).unwrap();
    let mut stream = send_request.send_request(request).await.expect("request");
    stream.finish().await.expect("finish");
    let _response = stream.recv_response().await.expect("response");
    while let Some(_chunk) = stream.recv_data().await.expect("data") {}
}

async fn issue_batch(
    send_request: &mut h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>,
    n: usize,
) {
    for _ in 0..n {
        issue_one(send_request).await;
    }
}

fn bench_h3_server(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let (server_ep, client_ep, client_conn, server_conn) = rt.block_on(loopback_pair());
    let cancel = CancellationToken::new();
    let _server_thread = spawn_native_server(server_conn, Http3Options::default(), cancel, handler);

    let (mut send_request, _driver) = rt.block_on(async {
        let (mut conn, send_request) = h3::client::builder()
            .build(h3_quinn::Connection::new(client_conn))
            .await
            .expect("h3 client");
        let driver = tokio::spawn(async move {
            let _ = conn.wait_idle().await;
            std::future::pending::<()>().await;
        });
        (send_request, driver)
    });

    const BATCH: usize = 100;
    let mut group = c.benchmark_group("h3_server");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(5));
    group.throughput(Throughput::Elements(BATCH as u64));
    group.bench_function("requests", |b| {
        b.iter(|| {
            rt.block_on(issue_batch(&mut send_request, BATCH));
        });
    });
    group.finish();

    // Latency percentiles: a manual probe so the baseline can be recorded in
    // the commit message (criterion does not print p99 by default).
    const N: usize = 1000;
    let mut latencies = Vec::with_capacity(N);
    for _ in 0..N {
        let start = Instant::now();
        rt.block_on(issue_one(&mut send_request));
        latencies.push(start.elapsed());
    }
    latencies.sort();
    let mean = latencies.iter().sum::<Duration>() / N as u32;
    let p50 = latencies[N / 2];
    let p99 = latencies[(N as f64 * 0.99) as usize];
    println!(
        "h3_server latency: mean={:?} p50={:?} p99={:?}",
        mean, p50, p99
    );

    // Keep the endpoints (and thus the connection) alive until here.
    drop(server_ep);
    drop(client_ep);
}

criterion_group!(benches, bench_h3_server);
criterion_main!(benches);
