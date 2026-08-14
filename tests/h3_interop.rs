//! Interop test: real `h3` client (over quinn) and a raw quinn fixture
//! client against the **native** HTTP/3 server over a real QUIC loopback
//! connection.
//!
//! The server runs on the `vibeio` runtime in its own thread (the native
//! driver spawns per-request tasks via `vibeio::spawn`); both quinn
//! endpoints live on the `tokio` runtime of the test thread, which pumps
//! the loopback. The `h3`-crate scenarios exercise request/response
//! streaming, trailers, concurrency, cancellation and graceful shutdown
//! against the reference implementation; the fixture-client scenarios pin
//! wire behavior (control/QPACK streams, SETTINGS, `100 Continue` and
//! `103 Early Hints` interims, a CONNECT request without `:scheme`/
//! `:path`) with hand-crafted frames.

#![cfg(feature = "h3-quinn")]

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    pin::Pin,
    sync::{mpsc, Arc},
    time::Duration,
};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use quinn::Endpoint;
use tokio_util::sync::CancellationToken;
use vibeio::RuntimeBuilder;
use vibeio_http::{
    qpack::{Decoder, Encoder},
    Frame, FrameDecoder, Http3, Http3Options, HttpProtocol, Incoming, Settings,
};

const H3_NO_ERROR: u64 = 0x0100;

fn loopback() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
}

/// A connected loopback pair plus the endpoints that keep both halves
/// alive (quinn tears the connections down if the endpoint is dropped).
async fn loopback_pair() -> (Endpoint, Endpoint, quinn::Connection, quinn::Connection) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_der: quinn::rustls::pki_types::CertificateDer<'static> = cert.cert.into();

    let server_config = quinn::ServerConfig::with_single_cert(
        vec![cert_der.clone()],
        quinn::rustls::pki_types::PrivateKeyDer::from(cert.signing_key),
    )
    .unwrap();
    let server_endpoint = Endpoint::server(server_config, loopback()).unwrap();
    let server_addr = server_endpoint.local_addr().unwrap();

    let mut roots = quinn::rustls::RootCertStore::empty();
    roots.add(cert_der).unwrap();
    let client_config = quinn::ClientConfig::with_root_certificates(Arc::new(roots)).unwrap();
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

/// Runs the native server on a `vibeio` runtime in a dedicated thread.
///
/// The server is shut down by cancelling `cancel`; the thread then exits
/// once the connection drains, and `handle`'s result is sent on the
/// returned channel.
fn spawn_native_server<F, Fut, ResB, ResBE, ResE>(
    server_conn: quinn::Connection,
    options: Http3Options,
    cancel: CancellationToken,
    handler: F,
) -> (
    std::thread::JoinHandle<()>,
    mpsc::Receiver<Result<(), std::io::Error>>,
)
where
    F: Fn(Request<Incoming>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<Response<ResB>, ResE>> + Send + 'static,
    ResB: http_body::Body<Data = Bytes, Error = ResBE> + Send + Unpin + 'static,
    ResBE: std::error::Error + Send + 'static,
    ResE: std::error::Error + Send + 'static,
{
    let (result_tx, result_rx) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        let rt = RuntimeBuilder::new()
            .enable_timer(true)
            .build()
            .expect("vibeio runtime");
        let result = rt.block_on(async move {
            Http3::new(vibeio_http::quinn::Connection::new(server_conn), options)
                .graceful_shutdown_token(cancel)
                .handle(handler)
                .await
        });
        result_tx.send(result).ok();
    });
    (thread, result_rx)
}

/// Drains the request body, collecting data and any trailers.
async fn read_body_and_trailers(body: Incoming) -> (Vec<u8>, Option<HeaderMap>) {
    let mut body = Pin::from(Box::new(body));
    let mut bytes = Vec::new();
    let mut trailers = None;
    while let Some(frame) =
        std::future::poll_fn(|cx| <Incoming as http_body::Body>::poll_frame(body.as_mut(), cx))
            .await
    {
        match frame {
            Ok(frame) => {
                if let Some(data) = frame.data_ref() {
                    bytes.extend_from_slice(data.as_ref());
                } else if let Some(t) = frame.trailers_ref() {
                    trailers = Some(t.clone());
                }
            }
            Err(_) => break,
        }
    }
    (bytes, trailers)
}

/// RFC 9000 Section 16 variable-length integer encoding.
fn write_varint(dst: &mut BytesMut, value: u64) {
    match value {
        0..=63 => dst.put_u8(value as u8),
        64..=16_383 => {
            dst.put_u8(0x40 | (value >> 8) as u8);
            dst.put_u8(value as u8);
        }
        16_384..=1_073_741_823 => {
            dst.put_u8(0x80 | (value >> 24) as u8);
            dst.put_u32(value as u32);
        }
        _ => {
            dst.put_u8(0xc0 | (value >> 56) as u8);
            dst.put_u64(value);
        }
    }
}

/// A raw quinn client that speaks HTTP/3 at the frame level: it opens the
/// control, QPACK encoder and decoder streams with their preludes and
/// SETTINGS, then sends hand-encoded request HEADERS/DATA on request
/// streams. The QPACK codecs are this crate's own, so the fixture pins the
/// wire behavior of the native implementation end to end.
struct Fixture {
    conn: quinn::Connection,
    encoder: Encoder,
    decoder: Decoder,
    /// Kept alive for the fixture's lifetime: quinn auto-FINishes a
    /// `SendStream` on drop, and closing a critical stream is a connection
    /// error for the server.
    #[allow(dead_code)]
    control: quinn::SendStream,
    encoder_stream: quinn::SendStream,
    #[allow(dead_code)]
    decoder_stream: quinn::SendStream,
    started_at: std::time::Instant,
}

impl Fixture {
    async fn new(conn: quinn::Connection) -> Self {
        let mut control = conn.open_uni().await.expect("control stream");
        let mut encoder_stream = conn.open_uni().await.expect("encoder stream");
        let mut decoder_stream = conn.open_uni().await.expect("decoder stream");

        let mut buf = BytesMut::new();
        write_varint(&mut buf, 0x00);
        Frame::Settings(Settings::new()).encode(&mut buf);
        control.write_all(&buf).await.expect("write settings");

        let mut buf = BytesMut::new();
        write_varint(&mut buf, 0x02);
        encoder_stream
            .write_all(&buf)
            .await
            .expect("encoder prelude");

        let mut buf = BytesMut::new();
        write_varint(&mut buf, 0x03);
        decoder_stream
            .write_all(&buf)
            .await
            .expect("decoder prelude");

        Fixture {
            conn,
            encoder: Encoder::new(0, false),
            decoder: Decoder::new(0, 0),
            control,
            encoder_stream,
            decoder_stream,
            started_at: std::time::Instant::now(),
        }
    }

    /// Opens a request stream and writes the HEADERS frame for `headers`.
    /// The send half is returned unfinished so the caller can stream DATA.
    async fn request_open(
        &mut self,
        headers: &[(Bytes, Bytes)],
    ) -> (quinn::SendStream, quinn::RecvStream) {
        let (mut send, recv) = self.conn.open_bi().await.expect("open request stream");
        let section = self.encoder.encode_section(headers);
        if !section.encoder_stream.is_empty() {
            self.encoder_stream
                .write_all(&section.encoder_stream)
                .await
                .expect("encoder instructions");
        }
        let mut buf = BytesMut::new();
        Frame::Headers(section.block).encode(&mut buf);
        send.write_all(&buf).await.expect("write headers");
        (send, recv)
    }

    /// Writes one DATA frame.
    async fn write_data(send: &mut quinn::SendStream, data: &[u8]) {
        let mut buf = BytesMut::new();
        Frame::Data(Bytes::copy_from_slice(data)).encode(&mut buf);
        send.write_all(&buf).await.expect("write data");
    }

    /// Decodes a response HEADERS block. The server is advertised a QPACK
    /// table capacity of 0, so blocks decode statelessly.
    fn decode(&mut self, block: &Bytes, stream_id: u64) -> Vec<(Bytes, Bytes)> {
        let now = self.started_at.elapsed().as_nanos() as u64;
        self.decoder
            .decode_block(block, stream_id, now)
            .expect("decode response headers")
            .expect("blocked without a dynamic table")
    }
}

/// Incremental frame reader over a quinn receive stream.
struct FrameReader {
    decoder: FrameDecoder,
}

impl FrameReader {
    fn new() -> Self {
        Self {
            decoder: FrameDecoder::new(),
        }
    }

    async fn next(&mut self, recv: &mut quinn::RecvStream) -> Option<Frame> {
        loop {
            match self.decoder.next_frame() {
                Ok(Some(frame)) => return Some(frame),
                Ok(None) => match recv.read_chunk(64 * 1024, true).await.expect("read chunk") {
                    Some(chunk) => self.decoder.extend(chunk.bytes),
                    None => return None,
                },
                Err(err) => panic!("fixture frame decode failed: {err:?}"),
            }
        }
    }
}

/// Reads the whole response: all HEADERS sections (decoded) plus the DATA.
async fn fixture_response(
    fixture: &mut Fixture,
    mut recv: quinn::RecvStream,
) -> (Vec<HashMap<String, String>>, Vec<u8>) {
    let stream_id = u64::from(recv.id());
    let mut reader = FrameReader::new();
    let mut sections = Vec::new();
    let mut data = Vec::new();
    while let Some(frame) = reader.next(&mut recv).await {
        match frame {
            Frame::Headers(block) => sections.push(field_map(fixture.decode(&block, stream_id))),
            Frame::Data(chunk) => data.extend_from_slice(&chunk),
            other => panic!("unexpected frame on request stream: {other:?}"),
        }
    }
    (sections, data)
}

fn field_map(fields: Vec<(Bytes, Bytes)>) -> HashMap<String, String> {
    fields
        .into_iter()
        .map(|(name, value)| {
            (
                String::from_utf8_lossy(&name).into_owned(),
                String::from_utf8_lossy(&value).into_owned(),
            )
        })
        .collect()
}

fn pseudo(headers: &[(&str, &str)]) -> Vec<(Bytes, Bytes)> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                Bytes::copy_from_slice(name.as_bytes()),
                Bytes::copy_from_slice(value.as_bytes()),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn h3_client_get_post_trailers_and_date() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let (_server_ep, _client_ep, client_conn, server_conn) = loopback_pair().await;
        let cancel = CancellationToken::new();
        let (server_thread, server_result) =
            spawn_native_server(
                server_conn,
                Http3Options::new(),
                cancel.clone(),
                |request| async move {
                    let (parts, body) = request.into_parts();

                    let (bytes, trailers) = read_body_and_trailers(body).await;

                    let trailer = trailers
                        .as_ref()
                        .and_then(|t| t.get("x-request-trailer"))
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("none");
                    let payload = format!(
                        "{} {} {} {}",
                        parts.method,
                        parts.uri.path(),
                        bytes.len(),
                        trailer
                    );
                    let mut response_trailers = HeaderMap::new();
                    response_trailers
                        .insert("x-response-trailer", HeaderValue::from_static("echoed"));
                    let body = Full::new(Bytes::from(payload)).with_trailers(std::future::ready(
                        Some(Ok::<HeaderMap, std::convert::Infallible>(response_trailers)),
                    ));
                    Ok::<_, std::convert::Infallible>(
                        Response::builder()
                            .status(StatusCode::OK)
                            .body(body)
                            .expect("response"),
                    )
                },
            );

        let (mut conn, mut send_request) = h3::client::builder()
            .build(h3_quinn::Connection::new(client_conn))
            .await
            .expect("h3 client");
        let drive = tokio::spawn(async move {
            let _ = conn.wait_idle().await;
            std::future::pending::<()>().await;
        });

        let request = Request::get("https://localhost/echo").body(()).unwrap();
        let mut stream = send_request.send_request(request).await.expect("GET");

        stream.finish().await.expect("finish GET");
        let response = stream.recv_response().await.expect("GET response");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.version(), http::Version::HTTP_3);
        assert!(
            response.headers().contains_key(http::header::DATE),
            "native server adds a Date header"
        );
        let mut body = Vec::new();
        while let Some(chunk) = stream.recv_data().await.expect("GET data") {
            body.extend_from_slice(chunk.chunk());
        }
        assert_eq!(body, b"GET /echo 0 none");

        assert_eq!(
            stream.recv_trailers().await.expect("GET trailers"),
            Some({
                let mut trailers = HeaderMap::new();
                trailers.insert("x-response-trailer", HeaderValue::from_static("echoed"));
                trailers
            })
        );

        let mut request = Request::post("https://localhost/post").body(()).unwrap();
        request
            .headers_mut()
            .insert("x-request-trailer", HeaderValue::from_static("yes"));

        let mut stream = send_request.send_request(request).await.expect("POST");

        stream
            .send_data(Bytes::from_static(b"hello "))
            .await
            .expect("POST data 1");

        stream
            .send_data(Bytes::from_static(b"world"))
            .await
            .expect("POST data 2");

        let mut trailers = HeaderMap::new();
        trailers.insert("x-request-trailer", HeaderValue::from_static("yes"));
        stream.send_trailers(trailers).await.expect("POST trailers");

        stream.finish().await.expect("POST finish");

        let response = stream.recv_response().await.expect("POST response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_LENGTH)
                .map(|v| v.to_str().unwrap()),
            Some("17")
        );
        let mut body = Vec::new();
        while let Some(chunk) = stream.recv_data().await.expect("POST data") {
            body.extend_from_slice(chunk.chunk());
        }
        assert_eq!(body, b"POST /post 11 yes");
        assert_eq!(
            stream.recv_trailers().await.expect("POST trailers"),
            Some({
                let mut trailers = HeaderMap::new();
                trailers.insert("x-response-trailer", HeaderValue::from_static("echoed"));
                trailers
            })
        );

        drop(stream);
        cancel.cancel();
        let result = server_result
            .recv_timeout(Duration::from_secs(10))
            .expect("server finished");
        assert!(result.is_ok(), "server handle: {result:?}");
        drive.abort();
        drop(send_request);
        server_thread.join().expect("join server");
    })
    .await
    .expect("h3_client_get_post_trailers_and_date timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn h3_client_concurrent_streams_and_abort() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let (_server_ep, _client_ep, client_conn, server_conn) = loopback_pair().await;
        let cancel = CancellationToken::new();
        let (server_thread, server_result) = spawn_native_server(
            server_conn,
            Http3Options::new(),
            cancel.clone(),
            |request| async move {
                let (_, body) = request.into_parts();
                let (bytes, _) = read_body_and_trailers(body).await;
                let payload = format!("{}", bytes.len());
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(Full::new(Bytes::from(payload)))
                        .expect("response"),
                )
            },
        );

        let (mut conn, mut send_request) = h3::client::builder()
            .build(h3_quinn::Connection::new(client_conn))
            .await
            .expect("h3 client");
        let drive = tokio::spawn(async move {
            let _ = conn.wait_idle().await;
            std::future::pending::<()>().await;
        });

        let mut streams = Vec::new();
        for i in 0..25usize {
            let request = Request::post("https://localhost/echo").body(()).unwrap();
            let mut stream = send_request
                .send_request(request)
                .await
                .expect("send request");
            stream
                .send_data(Bytes::from(vec![b'x'; i * 100]))
                .await
                .expect("send data");
            stream.finish().await.expect("finish");
            streams.push((i, stream));
        }

        let request = Request::post("https://localhost/abort").body(()).unwrap();
        let mut abort = send_request
            .send_request(request)
            .await
            .expect("abort request");
        abort
            .send_data(Bytes::from_static(b"partial body"))
            .await
            .expect("abort data");
        abort.stop_sending(h3::error::Code::H3_REQUEST_CANCELLED);
        drop(abort);

        for (i, mut stream) in streams {
            let response = stream.recv_response().await.expect("response");
            assert_eq!(response.status(), StatusCode::OK);
            let mut body = Vec::new();
            while let Some(chunk) = stream.recv_data().await.expect("data") {
                body.extend_from_slice(chunk.chunk());
            }
            assert_eq!(body, format!("{}", i * 100).as_bytes());
        }

        cancel.cancel();
        let result = server_result
            .recv_timeout(Duration::from_secs(10))
            .expect("server finished");
        assert!(result.is_ok(), "server handle: {result:?}");
        drive.abort();
        drop(send_request);
        server_thread.join().expect("join server");
    })
    .await
    .expect("h3_client_concurrent_streams_and_abort timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn h3_client_goaway_graceful_shutdown() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let (_server_ep, _client_ep, client_conn, server_conn) = loopback_pair().await;
        let cancel = CancellationToken::new();
        let (start_tx, start_rx) = mpsc::channel::<()>();
        let (server_thread, server_result) = spawn_native_server(
            server_conn,
            Http3Options::new(),
            cancel.clone(),
            move |request| {
                let start_tx = start_tx.clone();
                async move {
                    start_tx.send(()).ok();
                    let (_, body) = request.into_parts();
                    let (bytes, _) = read_body_and_trailers(body).await;
                    let payload = format!("got {} bytes", bytes.len());
                    Ok::<_, std::convert::Infallible>(
                        Response::builder()
                            .status(StatusCode::OK)
                            .body(Full::new(Bytes::from(payload)))
                            .expect("response"),
                    )
                }
            },
        );

        let client_raw = client_conn.clone();
        let (mut conn, mut send_request) = h3::client::builder()
            .build(h3_quinn::Connection::new(client_conn))
            .await
            .expect("h3 client");
        let drive = tokio::spawn(async move {
            let _ = conn.wait_idle().await;
            std::future::pending::<()>().await;
        });

        let request = Request::post("https://localhost/slow").body(()).unwrap();
        let mut a: h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes> =
            send_request.send_request(request).await.expect("request A");
        a.send_data(Bytes::from_static(b"first chunk"))
            .await
            .expect("A data");
        start_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("handler started");

        cancel.cancel();
        a.send_data(Bytes::from_static(b"rest chunk"))
            .await
            .expect("A rest data");
        a.finish().await.expect("finish A");

        let response = a.recv_response().await.expect("A response");
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = Vec::new();
        while let Some(chunk) = a.recv_data().await.expect("A data") {
            body.extend_from_slice(chunk.chunk());
        }
        assert_eq!(body, b"got 21 bytes");
        let closed = client_raw.closed().await;
        match closed {
            quinn::ConnectionError::ApplicationClosed(close) => {
                assert_eq!(close.error_code.into_inner(), H3_NO_ERROR);
            }
            other => panic!("unexpected close: {other:?}"),
        }

        drop(a);
        drive.abort();
        drop(send_request);
        let result = server_result
            .recv_timeout(Duration::from_secs(10))
            .expect("server finished");
        assert!(result.is_ok(), "server handle: {result:?}");
        server_thread.join().expect("join server");
    })
    .await
    .expect("h3_client_goaway_graceful_shutdown timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fixture_client_wire_get() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let (_server_ep, _client_ep, client_conn, server_conn) = loopback_pair().await;
        let cancel = CancellationToken::new();
        let (server_thread, server_result) = spawn_native_server(
            server_conn,
            Http3Options::new(),
            cancel.clone(),
            |request| async move {
                let (parts, _) = request.into_parts();
                let payload = format!("wire-ok {}", parts.uri.path());
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(Full::new(Bytes::from(payload)))
                        .expect("response"),
                )
            },
        );

        let mut fixture = Fixture::new(client_conn).await;
        let (mut send, recv) = fixture
            .request_open(&pseudo(&[
                (":method", "GET"),
                (":scheme", "https"),
                (":authority", "localhost"),
                (":path", "/wire"),
            ]))
            .await;
        send.finish().expect("finish request");

        let (sections, data) = fixture_response(&mut fixture, recv).await;
        assert_eq!(sections.len(), 1, "no interim responses for a plain GET");
        let headers = &sections[0];
        assert_eq!(headers.get(":status").map(String::as_str), Some("200"));
        assert_eq!(
            headers.get("content-length").map(String::as_str),
            Some("13")
        );
        assert!(headers.contains_key("date"), "native server adds Date");
        assert_eq!(data, b"wire-ok /wire");

        cancel.cancel();
        let result = server_result
            .recv_timeout(Duration::from_secs(10))
            .expect("server finished");
        assert!(result.is_ok(), "server handle: {result:?}");
        server_thread.join().expect("join server");
    })
    .await
    .expect("fixture_client_wire_get timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fixture_client_expect_100_continue() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let (_server_ep, _client_ep, client_conn, server_conn) = loopback_pair().await;
        let cancel = CancellationToken::new();
        let (server_thread, server_result) = spawn_native_server(
            server_conn,
            Http3Options::new(),
            cancel.clone(),
            |request| async move {
                let (_, body) = request.into_parts();
                let (bytes, _) = read_body_and_trailers(body).await;
                let payload = format!("got {} bytes", bytes.len());
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(Full::new(Bytes::from(payload)))
                        .expect("response"),
                )
            },
        );

        let mut fixture = Fixture::new(client_conn).await;
        let (mut send, mut recv) = fixture
            .request_open(&pseudo(&[
                (":method", "POST"),
                (":scheme", "https"),
                (":authority", "localhost"),
                (":path", "/continue"),
                ("expect", "100-continue"),
            ]))
            .await;
        let stream_id = u64::from(recv.id());
        let mut reader = FrameReader::new();

        let first = reader.next(&mut recv).await.expect("interim response");
        let Frame::Headers(block) = first else {
            panic!("first frame is not HEADERS: {first:?}");
        };
        let interim = fixture.decode(&block, stream_id);
        assert_eq!(
            interim
                .iter()
                .find(|(name, _)| name.as_ref() == b":status")
                .map(|(_, value)| value.as_ref()),
            Some(b"100".as_slice()),
            "server sends 100 Continue before the body is sent"
        );

        Fixture::write_data(&mut send, b"continue-body").await;
        send.finish().expect("finish request");

        let (sections, data) = fixture_response(&mut fixture, recv).await;
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].get(":status").map(String::as_str), Some("200"));
        assert_eq!(data, b"got 13 bytes");

        cancel.cancel();
        let result = server_result
            .recv_timeout(Duration::from_secs(10))
            .expect("server finished");
        assert!(result.is_ok(), "server handle: {result:?}");
        server_thread.join().expect("join server");
    })
    .await
    .expect("fixture_client_expect_100_continue timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fixture_client_early_hints() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let (_server_ep, _client_ep, client_conn, server_conn) = loopback_pair().await;
        let cancel = CancellationToken::new();
        let (server_thread, server_result) = spawn_native_server(
            server_conn,
            Http3Options::new(),
            cancel.clone(),
            |mut request| async move {
                let mut link = HeaderMap::new();
                link.insert(
                    http::header::LINK,
                    HeaderValue::from_static("</style.css>; rel=preload; as=style"),
                );
                vibeio_http::send_early_hints(&mut request, link)
                    .await
                    .expect("early hints");
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(Full::new(Bytes::from_static(b"done")))
                        .expect("response"),
                )
            },
        );

        let mut fixture = Fixture::new(client_conn).await;
        let (mut send, recv) = fixture
            .request_open(&pseudo(&[
                (":method", "GET"),
                (":scheme", "https"),
                (":authority", "localhost"),
                (":path", "/hints"),
            ]))
            .await;
        send.finish().expect("finish request");

        let (sections, data) = fixture_response(&mut fixture, recv).await;
        assert_eq!(sections.len(), 2, "103 Early Hints then the final response");
        assert_eq!(sections[0].get(":status").map(String::as_str), Some("103"));
        assert_eq!(
            sections[0].get("link").map(String::as_str),
            Some("</style.css>; rel=preload; as=style")
        );
        assert_eq!(sections[1].get(":status").map(String::as_str), Some("200"));
        assert_eq!(data, b"done");

        cancel.cancel();
        let result = server_result
            .recv_timeout(Duration::from_secs(10))
            .expect("server finished");
        assert!(result.is_ok(), "server handle: {result:?}");
        server_thread.join().expect("join server");
    })
    .await
    .expect("fixture_client_early_hints timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fixture_client_connect() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let (_server_ep, _client_ep, client_conn, server_conn) = loopback_pair().await;
        let cancel = CancellationToken::new();
        let (server_thread, server_result) = spawn_native_server(
            server_conn,
            Http3Options::new().enable_connect_protocol(true),
            cancel.clone(),
            |request| async move {
                let (parts, _) = request.into_parts();
                let payload = format!("connect={}", parts.method == Method::CONNECT);
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(Full::new(Bytes::from(payload)))
                        .expect("response"),
                )
            },
        );

        let mut fixture = Fixture::new(client_conn).await;
        let (mut send, recv) = fixture
            .request_open(&pseudo(&[
                (":method", "CONNECT"),
                (":authority", "localhost:443"),
            ]))
            .await;
        send.finish().expect("finish request");

        let (sections, data) = fixture_response(&mut fixture, recv).await;
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].get(":status").map(String::as_str), Some("200"));
        assert_eq!(data, b"connect=true");

        cancel.cancel();
        let result = server_result
            .recv_timeout(Duration::from_secs(10))
            .expect("server finished");
        assert!(result.is_ok(), "server handle: {result:?}");
        server_thread.join().expect("join server");
    })
    .await
    .expect("fixture_client_connect timed out");
}
