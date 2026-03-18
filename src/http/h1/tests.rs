#![cfg(test)]

use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::{BodyExt, Empty, Full};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::http::{
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
        .write_all(b"POST / HTTP/1.0\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nTrailer: X-Something\r\n\r\nD\r\nHello, World!\r\n0\r\nX-Something: value\r\n\r\n")
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
