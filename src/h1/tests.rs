#![cfg(test)]

use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::{BodyExt, Empty, Full};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{
    early_hints,
    h1::{prepare_upgrade, Http1, Http1Options},
    HttpProtocol,
};

#[tokio::test]
async fn test_get_request() {
    tokio::task::LocalSet::new()
        .run_until(async move {
            let (client_io, server_io) = tokio::io::duplex(4096);

            let server = Http1::new(server_io, Http1Options::new().header_read_timeout(None));
            let server_task = tokio::task::spawn_local(server.handle(|_req| async {
                Ok::<_, http::Error>(
                    http::Response::builder()
                        .status(200)
                        .body(Full::new(bytes::Bytes::from_static(b"Hello, World!")))
                        .unwrap(),
                )
            }));

            let (mut client_reader, mut client_writer) = tokio::io::split(client_io);

            // Write a GET request
            client_writer
                .write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
                .await
                .unwrap();

            // Read the response
            let mut response_buf = Vec::new();
            client_reader.read_to_end(&mut response_buf).await.unwrap();

            // Assert the response
            assert!(response_buf.starts_with(b"HTTP/1.0 200 OK\r\n"));
            assert!(response_buf.ends_with(b"\r\n\r\nHello, World!"));

            server_task.await.unwrap().unwrap();
        })
        .await
}

#[tokio::test]
async fn test_post_request() {
    tokio::task::LocalSet::new()
        .run_until(async move {
            let (client_io, server_io) = tokio::io::duplex(4096);

            let server = Http1::new(server_io, Http1Options::new().header_read_timeout(None));
            let server_task = tokio::task::spawn_local(server.handle(|req| async {
                let body = req.into_body().collect().await.unwrap().to_bytes();
                Ok::<_, http::Error>(
                    http::Response::builder()
                        .status(200)
                        .body(Full::new(body))
                        .unwrap(),
                )
            }));

            let (mut client_reader, mut client_writer) = tokio::io::split(client_io);

            // Write a POST request
            client_writer
        .write_all(
            b"POST / HTTP/1.0\r\nHost: localhost\r\nContent-Length: 14\r\n\r\nHello, vibeio!",
        )
        .await
        .unwrap();

            // Read the response
            let mut response_buf = Vec::new();
            client_reader.read_to_end(&mut response_buf).await.unwrap();

            // Assert the response
            assert!(response_buf.starts_with(b"HTTP/1.0 200 OK\r\n"));
            assert!(response_buf.ends_with(b"\r\n\r\nHello, vibeio!"));

            server_task.await.unwrap().unwrap();
        })
        .await
}

#[tokio::test]
async fn test_keep_alive() {
    tokio::task::LocalSet::new()
        .run_until(async move {
            let (client_io, server_io) = tokio::io::duplex(4096);

            let server = Http1::new(server_io, Http1Options::new().header_read_timeout(None));
            let server_task = tokio::task::spawn_local(server.handle(|_req| async {
                Ok::<_, http::Error>(
                    http::Response::builder()
                        .status(200)
                        .body(Full::new(bytes::Bytes::from_static(b"Hello")))
                        .unwrap(),
                )
            }));

            let (mut client_reader, mut client_writer) = tokio::io::split(client_io);

            // Write first request
            client_writer
                .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .unwrap();

            // Read first response
            let mut response_buf = vec![0; 1024];
            let n = client_reader.read(&mut response_buf).await.unwrap();
            response_buf.truncate(n);
            assert!(response_buf.starts_with(b"HTTP/1.1 200 OK\r\n"));
            assert!(response_buf.ends_with(b"\r\n\r\nHello"));

            // Write second request
            let _ = client_writer
                .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .await;

            // Read second response
            let mut response_buf = vec![0; 1024];
            let n = client_reader.read(&mut response_buf).await.unwrap();
            response_buf.truncate(n);
            assert!(response_buf.starts_with(b"HTTP/1.1 200 OK\r\n"));
            assert!(response_buf.ends_with(b"\r\n\r\nHello"));

            // Close the client writer to signal the end of the connection
            client_writer.shutdown().await.unwrap();

            server_task.await.unwrap().unwrap();
        })
        .await
}

#[tokio::test]
async fn test_connection_close() {
    tokio::task::LocalSet::new()
        .run_until(async move {
            let (client_io, server_io) = tokio::io::duplex(4096);

            let server = Http1::new(server_io, Http1Options::new().header_read_timeout(None));
            let server_task = tokio::task::spawn_local(server.handle(|_req| async {
                Ok::<_, http::Error>(
                    http::Response::builder()
                        .status(200)
                        .body(Empty::<bytes::Bytes>::new())
                        .unwrap(),
                )
            }));

            let (mut client_reader, mut client_writer) = tokio::io::split(client_io);

            // Write a request with "Connection: close"
            client_writer
                .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();

            // Read the response
            let mut response_buf = Vec::new();
            client_reader.read_to_end(&mut response_buf).await.unwrap();

            // Assert the response
            assert!(response_buf.starts_with(b"HTTP/1.1 200 OK\r\n"));
            assert!(response_buf
                .windows(19)
                .any(|w| w == b"connection: close\r\n"));

            // The connection should be closed by the server. A read should return 0 bytes.
            let n = client_reader.read(&mut response_buf).await.unwrap();
            assert_eq!(n, 0);

            server_task.await.unwrap().unwrap();
        })
        .await
}

#[tokio::test]
async fn test_fragmented() {
    tokio::task::LocalSet::new()
        .run_until(async move {
            let (client_io, server_io) = tokio::io::duplex(16);

            let server = Http1::new(server_io, Http1Options::new().header_read_timeout(None));
            let server_task = tokio::task::spawn_local(server.handle(|_req| async {
                Ok::<_, http::Error>(
                    http::Response::builder()
                        .status(200)
                        .body(Full::new(bytes::Bytes::from_static(b"Hello, World!")))
                        .unwrap(),
                )
            }));

            let (mut client_reader, mut client_writer) = tokio::io::split(client_io);

            tokio::task::spawn_local(async move {
                // Write a GET request
                client_writer
                    .write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
                    .await
                    .unwrap();
            });

            // Read the response
            let mut response_buf = Vec::new();
            client_reader.read_to_end(&mut response_buf).await.unwrap();

            println!(
                "response: {:?}",
                std::str::from_utf8(&response_buf).unwrap()
            );
            // Assert the response
            assert!(response_buf.starts_with(b"HTTP/1.0 200 OK\r\n"));
            assert!(response_buf.ends_with(b"\r\n\r\nHello, World!"));

            server_task.await.unwrap().unwrap();
        })
        .await
}

#[tokio::test]
async fn test_fragmented_non_vectored() {
    tokio::task::LocalSet::new()
        .run_until(async move {
            let (client_io, server_io) = tokio::io::duplex(16);

            let server = Http1::new(
                server_io,
                Http1Options::new()
                    .header_read_timeout(None)
                    .enable_vectored_write(false),
            );
            let server_task = tokio::task::spawn_local(server.handle(|_req| async {
                Ok::<_, http::Error>(
                    http::Response::builder()
                        .status(200)
                        .body(Full::new(bytes::Bytes::from_static(b"Hello, World!")))
                        .unwrap(),
                )
            }));

            let (mut client_reader, mut client_writer) = tokio::io::split(client_io);

            tokio::task::spawn_local(async move {
                // Write a GET request
                client_writer
                    .write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
                    .await
                    .unwrap();
            });

            // Read the response
            let mut response_buf = Vec::new();
            client_reader.read_to_end(&mut response_buf).await.unwrap();

            println!(
                "response: {:?}",
                std::str::from_utf8(&response_buf).unwrap()
            );
            // Assert the response
            assert!(response_buf.starts_with(b"HTTP/1.0 200 OK\r\n"));
            assert!(response_buf.ends_with(b"\r\n\r\nHello, World!"));

            server_task.await.unwrap().unwrap();
        })
        .await
}

#[tokio::test]
async fn test_chunked_response() {
    tokio::task::LocalSet::new()
        .run_until(async move {
            let (client_io, server_io) = tokio::io::duplex(4096);

            let server = Http1::new(server_io, Http1Options::new().header_read_timeout(None));
            let server_task = tokio::task::spawn_local(server.handle(|_req| async {
                Ok::<_, http::Error>(
                    http::Response::builder()
                        .status(200)
                        .header(http::header::TRANSFER_ENCODING, "chunked")
                        .body(Full::new(bytes::Bytes::from_static(b"Hello, World!")))
                        .unwrap(),
                )
            }));

            let (mut client_reader, mut client_writer) = tokio::io::split(client_io);

            // Write a GET request
            client_writer
                .write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
                .await
                .unwrap();

            // Read the response
            let mut response_buf = Vec::new();
            client_reader.read_to_end(&mut response_buf).await.unwrap();

            // Assert the response
            assert!(response_buf.starts_with(b"HTTP/1.0 200 OK\r\n"));
            assert!(response_buf.ends_with(b"\r\n\r\nD\r\nHello, World!\r\n0\r\n\r\n"));

            server_task.await.unwrap().unwrap();
        })
        .await
}

#[tokio::test]
async fn test_chunked_response_trailers() {
    tokio::task::LocalSet::new()
        .run_until(async move {
            let (client_io, server_io) = tokio::io::duplex(4096);

            let server = Http1::new(server_io, Http1Options::new().header_read_timeout(None));
            let server_task = tokio::task::spawn_local(server.handle(|_req| async {
                // Construct ody with trailers
                let mut trailers = http::HeaderMap::new();
                trailers.insert(http::header::CONTENT_TYPE, "text/plain".parse().unwrap());
                let data_stream = futures_util::stream::once(async move {
                    Ok::<_, std::io::Error>(http_body::Frame::data(Bytes::from_static(
                        b"Hello, World!",
                    )))
                });
                let trailer_stream =
                    futures_util::stream::once(
                        async move { Ok(http_body::Frame::trailers(trailers)) },
                    );
                let stream = data_stream.chain(trailer_stream);
                let body = http_body_util::BodyExt::boxed(http_body_util::StreamBody::new(stream));

                Ok::<_, http::Error>(
                    http::Response::builder()
                        .status(200)
                        .header(http::header::TRANSFER_ENCODING, "chunked")
                        .body(body)
                        .unwrap(),
                )
            }));
            let (mut client_reader, mut client_writer) = tokio::io::split(client_io);

            // Write a GET request
            client_writer
                .write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\nTE: trailers\r\n\r\n")
                .await
                .unwrap();

            // Read the response
            let mut response_buf = Vec::new();
            client_reader.read_to_end(&mut response_buf).await.unwrap();

            // Assert the response
            assert!(response_buf.starts_with(b"HTTP/1.0 200 OK\r\n"));
            assert!(response_buf
                .ends_with(b"\r\n\r\nD\r\nHello, World!\r\n0\r\ncontent-type: text/plain\r\n\r\n"));

            server_task.await.unwrap().unwrap();
        })
        .await
}

#[tokio::test]
async fn test_chunked_request() {
    tokio::task::LocalSet::new()
        .run_until(async move {
            let (client_io, server_io) = tokio::io::duplex(4096);

    let server = Http1::new(server_io, Http1Options::new().header_read_timeout(None));
    let server_task = tokio::task::spawn_local(server.handle(|req| async {
        let body = req.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&*body, b"Hello, World!");
        Ok::<_, http::Error>(http::Response::new(Full::new(bytes::Bytes::from_static(
            b"",
        ))))
    }));

    let (mut client_reader, mut client_writer) = tokio::io::split(client_io);

    // Write a POST request
    client_writer
        .write_all(b"POST / HTTP/1.0\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\nD\r\nHello, World!\r\n0\r\n\r\n")
        .await
        .unwrap();

    // Read the response
    let mut response_buf = Vec::new();
    client_reader.read_to_end(&mut response_buf).await.unwrap();

    // Assert the response
    assert!(response_buf.starts_with(b"HTTP/1.0 200 OK\r\n"));

    server_task.await.unwrap().unwrap();})
        .await
}

#[tokio::test]
async fn test_chunked_request_trailers() {
    tokio::task::LocalSet::new()
        .run_until(async move {
            let (client_io, server_io) = tokio::io::duplex(4096);

            let server = Http1::new(server_io, Http1Options::new().header_read_timeout(None));
            let server_task = tokio::task::spawn_local(server.handle(|req| async {
                let body = req.into_body().collect().await.unwrap();
                assert!(body.trailers().is_some());
                assert_eq!(
                    body.trailers()
                        .unwrap()
                        .get("X-Something")
                        .map(|v| v.as_bytes()),
                    Some(&b"value"[..])
                );
                assert_eq!(&*body.to_bytes(), b"Hello, World!");
                Ok::<_, http::Error>(http::Response::new(Full::new(bytes::Bytes::from_static(
                    b"",
                ))))
            }));

            let (mut client_reader, mut client_writer) = tokio::io::split(client_io);

            // Write a POST request
            client_writer
                .write_all(
                    b"POST / HTTP/1.0\r\nHost: localhost\
                      \r\nTransfer-Encoding: chunked\r\nTrailer: X-Something\r\n\r\nD\r\nHello, World!\r\n0\
                      \r\nX-Something: value\r\n\r\n",
                )
                .await
                .unwrap();

            // Read the response
            let mut response_buf = Vec::new();
            client_reader.read_to_end(&mut response_buf).await.unwrap();

            // Assert the response
            assert!(response_buf.starts_with(b"HTTP/1.0 200 OK\r\n"));

            server_task.await.unwrap().unwrap();
        })
        .await
}

#[tokio::test]
async fn test_upgrade_request() {
    tokio::task::LocalSet::new()
        .run_until(async move {
            let (client_io, server_io) = tokio::io::duplex(4096);

    let server = Http1::new(server_io, Http1Options::new().header_read_timeout(None));
    let server_task = tokio::task::spawn_local(server.handle(|mut req| async move {
        let upgrade = prepare_upgrade(&mut req);
        tokio::task::spawn_local(async move {
            let upgraded = upgrade.unwrap().await.unwrap();
            let (mut reader, mut writer) = tokio::io::split(upgraded);
            let _ = tokio::io::copy(&mut reader, &mut writer).await;
        });
        Ok::<_, http::Error>(
            http::Response::builder()
                .status(101)
                .header("Upgrade", "echo")
                .header("Connection", "upgrade")
                .body(Empty::new())
                .unwrap(),
        )
    }));

    let (mut client_reader, mut client_writer) = tokio::io::split(client_io);

    // Write a GET request
    client_writer
        .write_all(
            b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: upgrade\r\nUpgrade: echo\r\n\r\nHello, World!",
        )
        .await
        .unwrap();
    let _ = client_writer.shutdown().await;

    // Read the response
    let mut response_buf = Vec::new();
    client_reader.read_to_end(&mut response_buf).await.unwrap();

    // Assert the response
    assert!(response_buf.starts_with(b"HTTP/1.1 101 Switching Protocols\r\n"));
    assert!(response_buf.ends_with(b"Hello, World!"));

    server_task.await.unwrap().unwrap();})
        .await
}

#[tokio::test]
async fn test_invalid_request_line() {
    tokio::task::LocalSet::new()
        .run_until(async move {
            let (client_io, server_io) = tokio::io::duplex(4096);

            let server = Http1::new(server_io, Http1Options::new().header_read_timeout(None));
            let server_task = tokio::task::spawn_local(server.handle(|_req| async {
                Ok::<_, http::Error>(
                    http::Response::builder()
                        .status(200)
                        .body(Full::new(bytes::Bytes::from_static(b"Hello")))
                        .unwrap(),
                )
            }));

            let (mut client_reader, mut client_writer) = tokio::io::split(client_io);

            // Write a malformed request line
            client_writer
                .write_all(b"GET / HTTP?/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .unwrap();

            // Read the response
            let mut response_buf = Vec::new();
            client_reader.read_to_end(&mut response_buf).await.unwrap();

            // Expect 400 Bad Request
            assert!(response_buf.starts_with(b"HTTP/1.1 400 Bad Request\r\n"));

            let _ = server_task.await;
        })
        .await
}

#[tokio::test]
async fn test_invalid_headers() {
    tokio::task::LocalSet::new()
        .run_until(async move {
            let (client_io, server_io) = tokio::io::duplex(4096);

            let server = Http1::new(server_io, Http1Options::new().header_read_timeout(None));
            let server_task = tokio::task::spawn_local(server.handle(|_req| async {
                Ok::<_, http::Error>(
                    http::Response::builder()
                        .status(200)
                        .body(Full::new(bytes::Bytes::from_static(b"Hello")))
                        .unwrap(),
                )
            }));

            let (mut client_reader, mut client_writer) = tokio::io::split(client_io);

            // Write a request with invalid headers
            client_writer
                .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nHeader without colon\r\n\r\n")
                .await
                .unwrap();

            // Read the response
            let mut response_buf = Vec::new();
            client_reader.read_to_end(&mut response_buf).await.unwrap();

            // Expect 400 Bad Request
            assert!(response_buf.starts_with(b"HTTP/1.1 400 Bad Request\r\n"));

            let _ = server_task.await;
        })
        .await
}

#[tokio::test]
async fn test_header_size_limit_exceeded() {
    tokio::task::LocalSet::new()
        .run_until(async move {
            let (client_io, server_io) = tokio::io::duplex(4096);

            let server = Http1::new(server_io, Http1Options::new().header_read_timeout(None));
            let server_task = tokio::task::spawn_local(server.handle(|req| async {
                let _ = req.into_body().collect().await;
                Ok::<_, http::Error>(
                    http::Response::builder()
                        .status(200)
                        .body(Full::new(bytes::Bytes::from_static(b"Hello")))
                        .unwrap(),
                )
            }));

            let (mut client_reader, mut client_writer) = tokio::io::split(client_io);

            // Write a request with a large header
            let large_header = "Header: ".to_string() + "x".repeat(1024 * 1024).as_str();
            let _ = client_writer
                .write_all(
                    format!(
                        "GET / HTTP/1.1\r\nHost: localhost\r\n{}\r\n\r\n",
                        large_header
                    )
                    .as_bytes(),
                )
                .await;

            // Read the response
            let mut response_buf = Vec::new();
            client_reader.read_to_end(&mut response_buf).await.unwrap();

            // Expect 400 Bad Request
            assert!(response_buf.starts_with(b"HTTP/1.1 400 Bad Request\r\n"));

            let _ = server_task.await;
        })
        .await;
}

#[tokio::test]
async fn test_http_pipelining() {
    tokio::task::LocalSet::new()
        .run_until(async move {
            let (client_io, server_io) = tokio::io::duplex(4096);

            let server = Http1::new(server_io, Http1Options::new().header_read_timeout(None));
            let server_task = tokio::task::spawn_local(server.handle(|req| async {
                let _ = req.into_body().collect().await;
                Ok::<_, http::Error>(
                    http::Response::builder()
                        .status(200)
                        .body(Full::new(bytes::Bytes::from_static(b"Hello")))
                        .unwrap(),
                )
            }));

            let (mut client_reader, mut client_writer) = tokio::io::split(client_io);

            // Write multiple requests in a single write
            let requests = vec![
                "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n",
                "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n",
            ];
            let _ = client_writer.write_all(requests.join("").as_bytes()).await;
            let _ = client_writer.shutdown().await;

            // Read the responses
            let mut response_buf = Vec::new();
            client_reader.read_to_end(&mut response_buf).await.unwrap();

            // Expect 200 OK for both responses
            assert!(response_buf.starts_with(b"HTTP/1.1 200 OK\r\n"));

            let _ = server_task.await;
        })
        .await;
}

#[tokio::test]
async fn test_early_hints_before_final_response() {
    tokio::task::LocalSet::new()
        .run_until(async move {
            let (client_io, server_io) = tokio::io::duplex(4096);

            let server = Http1::new(
                server_io,
                Http1Options::new()
                    .header_read_timeout(None)
                    .enable_early_hints(true),
            );
            let server_task = tokio::task::spawn_local(server.handle(|mut req| async {
                let mut early_hint_headers = http::HeaderMap::new();
                early_hint_headers.insert(
                    http::header::LINK,
                    http::header::HeaderValue::from_static(
                        "<https://cdn.example.com/style.css>; rel=preload; as=style, \
                        <https://api.example.com/next>; rel=next",
                    ),
                );
                let _ = early_hints::send_early_hints(&mut req, early_hint_headers).await;
                let _ = req.into_body().collect().await;
                Ok::<_, http::Error>(
                    http::Response::builder()
                        .status(200)
                        .body(Full::new(bytes::Bytes::from_static(b"Hello")))
                        .unwrap(),
                )
            }));

            let (mut client_reader, mut client_writer) = tokio::io::split(client_io);

            let _ = client_writer
                .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await;
            let _ = client_writer.shutdown().await;

            // Read the responses
            let mut response_buf = Vec::new();
            client_reader.read_to_end(&mut response_buf).await.unwrap();

            println!("response: {:?}", response_buf);
            // Expect 103 Early Hints followed by 200 OK for the final response
            assert!(response_buf.starts_with(b"HTTP/1.1 103 Early Hints\r\n"));
            assert!(response_buf
                .windows(21)
                .any(|w| w == b"\r\n\r\nHTTP/1.1 200 OK\r\n"));

            let _ = server_task.await;
        })
        .await;
}

#[tokio::test]
async fn test_chunked_overrides_content_length() {
    tokio::task::LocalSet::new()
        .run_until(async move {
            let (client_io, server_io) = tokio::io::duplex(4096);

            let server = Http1::new(server_io, Http1Options::new().header_read_timeout(None));
            let server_task = tokio::task::spawn_local(server.handle(|req| async {
                let body = req.into_body().collect().await.unwrap().to_bytes();
                assert_eq!(&*body, b"Hello, World!");
                Ok::<_, http::Error>(http::Response::new(Full::new(bytes::Bytes::from_static(
                    b"",
                ))))
            }));

            let (mut client_reader, mut client_writer) = tokio::io::split(client_io);

            // Write a POST request
            client_writer
                .write_all(
                    b"POST / HTTP/1.1\r\nHost: localhost\r\nConnection: close\
                     \r\nContent-Length: 13\r\nTransfer-Encoding: chunked\r\n\r\n\
                     D\r\nHello, World!\r\n0\r\n\r\n",
                )
                .await
                .unwrap();

            // Read the response
            let mut response_buf = Vec::new();
            client_reader.read_to_end(&mut response_buf).await.unwrap();

            // Assert the response
            assert!(response_buf.starts_with(b"HTTP/1.1 200 OK\r\n"));

            server_task.await.unwrap().unwrap();
        })
        .await
}

#[test]
fn test_slowloris() {
    // A "vibeio" runtime has to be built, since this test depends on the timer,
    // which can't be used with Tokio
    vibeio::RuntimeBuilder::new()
        .enable_timer(true)
        .build()
        .unwrap()
        .block_on(async move {
            let (client_io, server_io) = tokio::io::duplex(4096);

            let server = Http1::new(
                server_io,
                Http1Options::new().header_read_timeout(Some(Duration::from_millis(250))),
            );
            let server_task = vibeio::spawn(server.handle(|req| async {
                let _ = req.into_body().collect().await;
                Ok::<_, http::Error>(
                    http::Response::builder()
                        .status(200)
                        .body(Full::new(bytes::Bytes::from_static(b"Hello")))
                        .unwrap(),
                )
            }));

            let (mut client_reader, mut client_writer) = tokio::io::split(client_io);

            let client_task = vibeio::spawn(async move {
                // Simulate a slowloris attack by slowly writing the request
                let data = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";
                for chunk in data.chunks(4) {
                    vibeio::time::sleep(Duration::from_millis(50)).await;
                    if client_writer.write_all(chunk).await.is_err() {
                        break;
                    }
                }
            });

            // Read the response
            let mut response_buf = Vec::new();
            client_reader.read_to_end(&mut response_buf).await.unwrap();

            assert!(response_buf.starts_with(b"HTTP/1.1 408 Request Timeout\r\n"));

            let _ = client_task.cancel();
            let _ = server_task.await;
        });
}
