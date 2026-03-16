#![cfg(test)]

use http_body_util::{BodyExt, Empty, Full};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::http::{
    HttpProtocol,
    h1::{Http1, Http1Options},
};

#[tokio::test]
async fn test_get_request() {
    let (client_io, server_io) = tokio::io::duplex(4096);

    let server = Http1::new(server_io, Http1Options::new().header_read_timeout(None));
    let server_task = tokio::spawn(server.handle(|_req| async {
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
}

#[tokio::test]
async fn test_post_request() {
    let (client_io, server_io) = tokio::io::duplex(4096);

    let server = Http1::new(server_io, Http1Options::new().header_read_timeout(None));
    let server_task = tokio::spawn(server.handle(|req| async {
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
}

#[tokio::test]
async fn test_keep_alive() {
    let (client_io, server_io) = tokio::io::duplex(4096);

    let server = Http1::new(server_io, Http1Options::new().header_read_timeout(None));
    let server_task = tokio::spawn(server.handle(|_req| async {
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
    let n = client_reader.read(&mut response_buf).await.unwrap();
    response_buf.truncate(n);
    assert!(response_buf.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert!(response_buf.ends_with(b"\r\n\r\nHello"));

    // Close the client writer to signal the end of the connection
    client_writer.shutdown().await.unwrap();

    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn test_connection_close() {
    let (client_io, server_io) = tokio::io::duplex(4096);

    let server = Http1::new(server_io, Http1Options::new().header_read_timeout(None));
    let server_task = tokio::spawn(server.handle(|_req| async {
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
    assert!(
        response_buf
            .windows(19)
            .any(|w| w == b"connection: close\r\n")
    );

    // The connection should be closed by the server. A read should return 0 bytes.
    let n = client_reader.read(&mut response_buf).await.unwrap();
    assert_eq!(n, 0);

    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn test_fragmented() {
    let (client_io, server_io) = tokio::io::duplex(16);

    let server = Http1::new(server_io, Http1Options::new().header_read_timeout(None));
    let server_task = tokio::spawn(server.handle(|_req| async {
        Ok::<_, http::Error>(
            http::Response::builder()
                .status(200)
                .body(Full::new(bytes::Bytes::from_static(b"Hello, World!")))
                .unwrap(),
        )
    }));

    let (mut client_reader, mut client_writer) = tokio::io::split(client_io);

    tokio::spawn(async move {
        // Write a GET request
        client_writer
            .write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
    });

    // Read the response
    let mut response_buf = Vec::new();
    client_reader.read_to_end(&mut response_buf).await.unwrap();

    // Assert the response
    assert!(response_buf.starts_with(b"HTTP/1.0 200 OK\r\n"));
    assert!(response_buf.ends_with(b"\r\n\r\nHello, World!"));

    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn test_chunked_response() {
    let (client_io, server_io) = tokio::io::duplex(4096);

    let server = Http1::new(server_io, Http1Options::new().header_read_timeout(None));
    let server_task = tokio::spawn(server.handle(|_req| async {
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
}
