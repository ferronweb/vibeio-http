#![no_main]

use std::{io::Cursor, time::Duration};

use http_body_util::BodyExt;
use libfuzzer_sys::fuzz_target;
use tokio::io::AsyncWrite;
use vibeio_http::HttpProtocol;

struct AlwaysFailingAsyncWrite;

impl AsyncWrite for AlwaysFailingAsyncWrite {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Ready(Err(std::io::ErrorKind::Other.into()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

fuzz_target!(|data: &[u8]| {
    let data_owned = data.to_vec();

    // Use mock driver, because it's designed for testing purposes,
    // and in this fuzzing, no real I/O is involved.
    let rt = vibeio::RuntimeBuilder::new()
        .driver(vibeio::DriverKind::Mock)
        .build()
        .expect("Failed to build runtime");

    rt.block_on(async move {
        let input_io = Cursor::new(data_owned);
        let output_io = AlwaysFailingAsyncWrite;
        let io = tokio::io::join(input_io, output_io);

        let _ = vibeio_http::Http1::new(
            io,
            vibeio_http::Http1Options::default()
                .header_read_timeout(Some(Duration::from_millis(2))),
        )
        .handle(|req| async move {
            let (_parts, mut body) = req.into_parts();
            while body.frame().await.is_some() {}
            http::Response::builder()
                .status(200)
                .body(http_body_util::Empty::<bytes::Bytes>::new())
        })
        .await;
    })
});
