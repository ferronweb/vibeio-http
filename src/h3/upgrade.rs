// Copyright (c) 2014-2026 Sean McArthur
// Portions of this file are derived from `hyper` (https://github.com/hyperium/hyper).
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.

use std::{
    future::Future,
    pin::Pin,
    task::{ready, Context, Poll},
};

use bytes::{Buf, Bytes};
use h3::{
    quic::{RecvStream, SendStream},
    server::RequestStream,
};
use tokio::io::{AsyncWrite, ReadBuf};

use crate::h3::h3_stream_error_to_io;

#[inline]
pub(super) fn pair<SSend, SRecv>(
    send_stream: RequestStream<SSend, bytes::Bytes>,
    recv_stream: RequestStream<SRecv, bytes::Bytes>,
) -> (H3Upgraded<SRecv>, UpgradedSendStreamTask<SSend>) {
    let (tx, rx) = kanal::bounded_async(1);
    let (error_tx, error_rx) = oneshot::async_channel();

    (
        H3Upgraded {
            send_stream: UpgradedSendStreamBridge { tx, error_rx },
            recv_stream,
            buf: Bytes::new(),
        },
        UpgradedSendStreamTask {
            h3_tx: send_stream,
            h3_tx_fut: None,
            rx,
            error_tx: Some(error_tx),
        },
    )
}

pub(super) struct H3Upgraded<SR> {
    send_stream: UpgradedSendStreamBridge,
    recv_stream: RequestStream<SR, bytes::Bytes>,
    buf: Bytes,
}

impl<SR: RecvStream> tokio::io::AsyncRead for H3Upgraded<SR> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        read_buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        if self.buf.is_empty() {
            self.buf = loop {
                match ready!(self.recv_stream.poll_recv_data(cx)) {
                    Ok(None) => return Poll::Ready(Ok(())),
                    Ok(Some(buf)) if !buf.has_remaining() => continue,
                    Ok(Some(mut buf)) => {
                        break buf.copy_to_bytes(buf.remaining());
                    }
                    Err(e) => {
                        return Poll::Ready(if e.is_h3_no_error() {
                            Ok(())
                        } else {
                            Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))
                        })
                    }
                }
            };
        }
        let cnt = std::cmp::min(self.buf.len(), read_buf.remaining());
        read_buf.put_slice(&self.buf[..cnt]);
        self.buf.advance(cnt);
        Poll::Ready(Ok(()))
    }
}

impl<SR> AsyncWrite for H3Upgraded<SR> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let n = buf.len();
        match std::pin::pin!(self.send_stream.tx.send(buf.into())).poll(cx) {
            Poll::Ready(Ok(())) => return Poll::Ready(Ok(n)),
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(_task_dropped)) => {
                // if the task dropped, check if there was an error
                // otherwise i guess its a broken pipe
            }
        };
        match Pin::new(&mut self.send_stream.error_rx).poll(cx) {
            Poll::Ready(Ok(reason)) => {
                Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::Other, reason)))
            }
            Poll::Ready(Err(_task_dropped)) => {
                Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    #[inline]
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let _ = self.send_stream.tx.close();
        match Pin::new(&mut self.send_stream.error_rx).poll(cx) {
            Poll::Ready(Ok(reason)) => {
                Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::Other, reason)))
            }
            Poll::Ready(Err(_task_dropped)) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }
}

struct UpgradedSendStreamBridge {
    tx: kanal::AsyncSender<Box<[u8]>>,
    error_rx: oneshot::AsyncReceiver<std::io::Error>,
}

pub struct UpgradedSendStreamTask<S> {
    h3_tx: RequestStream<S, bytes::Bytes>,
    h3_tx_fut: Option<Pin<Box<dyn Future<Output = Result<(), h3::error::StreamError>>>>>,
    rx: kanal::AsyncReceiver<Box<[u8]>>,
    error_tx: Option<oneshot::Sender<std::io::Error>>,
}

impl<S: SendStream<bytes::Bytes> + 'static> UpgradedSendStreamTask<S> {
    #[inline]
    fn tick(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        let this = self.get_mut();

        // this is a manual `select()` over 3 "futures", so we always need
        // to be sure they are ready and/or we are waiting notification of
        // one of the sides hanging up, so the task doesn't live around
        // longer than it's meant to.
        loop {
            if let Some(mut fut) = this.h3_tx_fut.take() {
                match fut.as_mut().poll(cx) {
                    Poll::Ready(Ok(())) => {}
                    Poll::Ready(Err(e)) => {
                        return Poll::Ready(Err(h3_stream_error_to_io(e)));
                    }
                    Poll::Pending => {
                        this.h3_tx_fut = Some(fut);
                        return Poll::Pending;
                    }
                }
            }

            match std::pin::pin!(this.rx.recv()).poll(cx) {
                Poll::Ready(Ok(cursor)) => {
                    // Safety: "send_fut" would not outlive "this.h3_tx"
                    let send_fut = unsafe {
                        std::mem::transmute::<
                            &mut RequestStream<S, bytes::Bytes>,
                            &'static mut RequestStream<S, bytes::Bytes>,
                        >(&mut this.h3_tx)
                    }
                    .send_data(Bytes::from_owner(cursor));
                    this.h3_tx_fut = Some(Box::pin(send_fut));
                }
                Poll::Ready(Err(_)) => {
                    // Safety: "finish_fut" would not outlive "this.h3_tx"
                    let finish_fut = unsafe {
                        std::mem::transmute::<
                            &mut RequestStream<S, bytes::Bytes>,
                            &'static mut RequestStream<S, bytes::Bytes>,
                        >(&mut this.h3_tx)
                    }
                    .finish();
                    this.h3_tx_fut = Some(Box::pin(finish_fut));
                }
                Poll::Pending => {
                    return Poll::Pending;
                }
            }
        }
    }
}

impl<S: SendStream<bytes::Bytes> + 'static> Future for UpgradedSendStreamTask<S> {
    type Output = ();

    #[inline]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.as_mut().tick(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(()),
            Poll::Ready(Err(err)) => {
                if let Some(tx) = self.error_tx.take() {
                    let _oh_well = tx.send(err);
                }
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S> Drop for UpgradedSendStreamTask<S> {
    fn drop(&mut self) {
        let _ = self.h3_tx_fut.take();
    }
}
