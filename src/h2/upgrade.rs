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

use std::future::Future;
use std::io::Cursor;
use std::pin::Pin;
use std::task::{ready, Context, Poll};

use bytes::{Buf, Bytes};
use h2::{Reason, RecvStream, SendStream};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::h2::send::SendBuf;
use crate::{h2_error_to_io, h2_reason_to_io};

#[inline]
pub(super) fn pair<B>(
    send_stream: SendStream<SendBuf<B>>,
    recv_stream: RecvStream,
) -> (H2Upgraded, UpgradedSendStreamTask<B>) {
    let (tx, rx) = kanal::bounded_async(1);
    let (error_tx, error_rx) = oneshot::async_channel();

    (
        H2Upgraded {
            send_stream: UpgradedSendStreamBridge { tx, error_rx },
            recv_stream,
            buf: Bytes::new(),
        },
        UpgradedSendStreamTask {
            h2_tx: send_stream,
            rx,
            error_tx: Some(error_tx),
        },
    )
}

pub(super) struct H2Upgraded {
    send_stream: UpgradedSendStreamBridge,
    recv_stream: RecvStream,
    buf: Bytes,
}

struct UpgradedSendStreamBridge {
    tx: kanal::AsyncSender<Cursor<Box<[u8]>>>,
    error_rx: oneshot::AsyncReceiver<std::io::Error>,
}

pub struct UpgradedSendStreamTask<B> {
    h2_tx: SendStream<SendBuf<B>>,
    rx: kanal::AsyncReceiver<Cursor<Box<[u8]>>>,
    error_tx: Option<oneshot::Sender<std::io::Error>>,
}

// ===== impl UpgradedSendStreamTask =====

impl<B> UpgradedSendStreamTask<B>
where
    B: Buf,
{
    #[inline]
    fn tick(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        let this = self.get_mut();

        // this is a manual `select()` over 3 "futures", so we always need
        // to be sure they are ready and/or we are waiting notification of
        // one of the sides hanging up, so the task doesn't live around
        // longer than it's meant to.
        loop {
            // we don't have the next chunk of data yet, so just reserve 1 byte to make
            // sure there's some capacity available. h2 will handle the capacity management
            // for the actual body chunk.
            this.h2_tx.reserve_capacity(1);

            if this.h2_tx.capacity() == 0 {
                // poll_capacity oddly needs a loop
                'capacity: loop {
                    match this.h2_tx.poll_capacity(cx) {
                        Poll::Ready(Some(Ok(0))) => {}
                        Poll::Ready(Some(Ok(_))) => break,
                        Poll::Ready(Some(Err(e))) => {
                            return Poll::Ready(Err(std::io::Error::other(
                                e,
                            )))
                        }
                        Poll::Ready(None) => {
                            // None means the stream is no longer in a
                            // streaming state, we either finished it
                            // somehow, or the remote reset us.
                            return Poll::Ready(Err(std::io::Error::other(
                                "send stream capacity unexpectedly closed",
                            )));
                        }
                        Poll::Pending => break 'capacity,
                    }
                }
            }

            match this.h2_tx.poll_reset(cx) {
                Poll::Ready(Ok(reason)) => {
                    return Poll::Ready(Err(h2_reason_to_io(reason)));
                }
                Poll::Ready(Err(err)) => return Poll::Ready(Err(h2_error_to_io(err))),
                Poll::Pending => (),
            }

            match std::pin::pin!(this.rx.recv()).poll(cx) {
                Poll::Ready(Ok(cursor)) => {
                    this.h2_tx
                        .send_data(SendBuf::Cursor(cursor), false)
                        .map_err(std::io::Error::other)?;
                }
                Poll::Ready(Err(_)) => {
                    this.h2_tx
                        .send_data(SendBuf::None, true)
                        .map_err(std::io::Error::other)?;
                    return Poll::Ready(Ok(()));
                }
                Poll::Pending => {
                    return Poll::Pending;
                }
            }
        }
    }
}

impl<B> Future for UpgradedSendStreamTask<B>
where
    B: Buf,
{
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

// ===== impl H2Upgraded =====

impl AsyncRead for H2Upgraded {
    #[inline]
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        read_buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        if self.buf.is_empty() {
            self.buf = loop {
                match ready!(self.recv_stream.poll_data(cx)) {
                    None => return Poll::Ready(Ok(())),
                    Some(Ok(buf)) if buf.is_empty() && !self.recv_stream.is_end_stream() => {
                        continue
                    }
                    Some(Ok(buf)) => {
                        break buf;
                    }
                    Some(Err(e)) => {
                        return Poll::Ready(match e.reason() {
                            Some(Reason::NO_ERROR) | Some(Reason::CANCEL) => Ok(()),
                            Some(Reason::STREAM_CLOSED) => {
                                Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))
                            }
                            _ => Err(h2_error_to_io(e)),
                        })
                    }
                }
            };
        }
        let cnt = std::cmp::min(self.buf.len(), read_buf.remaining());
        read_buf.put_slice(&self.buf[..cnt]);
        self.buf.advance(cnt);
        let _ = self.recv_stream.flow_control().release_capacity(cnt);
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for H2Upgraded {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let n = buf.len();
        match std::pin::pin!(self.send_stream.tx.send(Cursor::new(buf.into()))).poll(cx) {
            Poll::Ready(Ok(())) => return Poll::Ready(Ok(n)),
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(_task_dropped)) => {
                // if the task dropped, check if there was an error
                // otherwise i guess its a broken pipe
            }
        };
        match Pin::new(&mut self.send_stream.error_rx).poll(cx) {
            Poll::Ready(Ok(reason)) => {
                Poll::Ready(Err(std::io::Error::other(reason)))
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
                Poll::Ready(Err(std::io::Error::other(reason)))
            }
            Poll::Ready(Err(_task_dropped)) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }
}
