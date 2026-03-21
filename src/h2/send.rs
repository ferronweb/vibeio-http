use std::{
    future::Future,
    io::Cursor,
    pin::Pin,
    task::{ready, Context, Poll},
};

use h2::SendStream;
use http_body::Body;

use crate::{h2_error_to_io, h2_reason_to_io};

pub(crate) struct PipeToSendStream<'a, S>
where
    S: Body,
{
    body_tx: SendStream<SendBuf<S::Data>>,
    stream: Pin<&'a mut S>,
    capacity_reserving: bool,
}

impl<'a, S> PipeToSendStream<'a, S>
where
    S: Body + Unpin,
{
    #[inline]
    pub fn new(body_tx: SendStream<SendBuf<S::Data>>, stream: &'a mut S) -> Self {
        Self {
            body_tx,
            stream: Pin::new(stream),
            capacity_reserving: false,
        }
    }
}

impl<'a, S> Future for PipeToSendStream<'a, S>
where
    S: Body + Unpin,
    S::Error: std::error::Error,
{
    type Output = Result<(), std::io::Error>;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        loop {
            if !this.capacity_reserving {
                this.body_tx.reserve_capacity(1);
                // Set a flag to avoid unnecessary reserve calls
                this.capacity_reserving = true;
            }

            if this.body_tx.capacity() == 0 {
                // Wait for capacity to become available
                loop {
                    match ready!(this.body_tx.poll_capacity(cx)) {
                        Some(Ok(0)) => {
                            // No capacity, wait for capacity to become available
                        }
                        Some(Ok(_)) => break,
                        Some(Err(e)) => return Poll::Ready(Err(h2_error_to_io(e))),
                        None => {
                            return Poll::Ready(Err(std::io::Error::other(
                                "send stream capacity unexpectedly closed",
                            )));
                        }
                    }
                }
            }

            this.capacity_reserving = false;

            match this.body_tx.poll_reset(cx) {
                Poll::Ready(Ok(reason)) => {
                    return Poll::Ready(Err(h2_reason_to_io(reason)));
                }
                Poll::Ready(Err(err)) => {
                    return Poll::Ready(Err(h2_error_to_io(err)));
                }
                Poll::Pending => {}
            }

            match ready!(this.stream.as_mut().poll_frame(cx)) {
                Some(Ok(frame)) => {
                    if frame.is_data() {
                        let chunk = frame.into_data().unwrap_or_else(|_| unreachable!());
                        let is_eos = this.stream.is_end_stream();

                        let buf = SendBuf::Buf(chunk);
                        this.body_tx
                            .send_data(buf, is_eos)
                            .map_err(h2_error_to_io)?;

                        if is_eos {
                            return Poll::Ready(Ok(()));
                        }
                    } else if frame.is_trailers() {
                        this.body_tx.reserve_capacity(0);
                        this.body_tx
                            .send_trailers(frame.into_trailers().unwrap_or_else(|_| unreachable!()))
                            .map_err(h2_error_to_io)?;
                        return Poll::Ready(Ok(()));
                    } else {
                        // Unknown frame, discard
                    }
                }
                Some(Err(e)) => return Poll::Ready(Err(std::io::Error::other(e.to_string()))),
                None => {
                    return Poll::Ready(
                        this.body_tx
                            .send_data(SendBuf::None, true)
                            .map_err(h2_error_to_io),
                    );
                }
            }
        }
    }
}

#[repr(usize)]
pub(super) enum SendBuf<B> {
    Buf(B),
    Cursor(Cursor<Box<[u8]>>),
    None,
}

impl<B: bytes::Buf> bytes::Buf for SendBuf<B> {
    #[inline]
    fn remaining(&self) -> usize {
        match self {
            Self::Buf(b) => b.remaining(),
            Self::Cursor(c) => c.remaining(),
            Self::None => 0,
        }
    }

    #[inline]
    fn chunk(&self) -> &[u8] {
        match self {
            Self::Buf(b) => b.chunk(),
            Self::Cursor(c) => c.get_ref(),
            Self::None => &[],
        }
    }

    #[inline]
    fn advance(&mut self, cnt: usize) {
        match self {
            Self::Buf(b) => b.advance(cnt),
            Self::Cursor(c) => c.advance(cnt),
            Self::None => {}
        }
    }

    #[inline]
    fn chunks_vectored<'a>(&'a self, dst: &mut [std::io::IoSlice<'a>]) -> usize {
        match self {
            Self::Buf(b) => b.chunks_vectored(dst),
            Self::Cursor(c) => c.chunks_vectored(dst),
            Self::None => 0,
        }
    }
}
