use std::{collections::VecDeque, io::IoSlice};

use bytes::BytesMut;
use tokio::io::AsyncWriteExt;

pub(super) struct WriteBuf {
    inner: VecDeque<IoSlice<'static>>,
}

impl WriteBuf {
    #[inline]
    pub fn new() -> Self {
        Self {
            inner: VecDeque::new(),
        }
    }

    #[inline]
    pub unsafe fn push(&mut self, ioslice: IoSlice<'_>) {
        self.inner
            .push_back(unsafe { std::mem::transmute::<IoSlice<'_>, IoSlice<'static>>(ioslice) });
    }

    #[inline]
    async unsafe fn flush_vectored_inner(
        &mut self,
        stream: &mut (impl tokio::io::AsyncWrite + Unpin),
    ) -> std::io::Result<()> {
        let mut bufs = self.inner.make_contiguous();
        while !bufs.is_empty() {
            let n = stream.write_vectored(bufs).await?;
            if n == 0 {
                return Err(std::io::ErrorKind::WriteZero.into());
            }
            IoSlice::advance_slices(&mut bufs, n);
        }
        self.inner.drain(..);
        Ok(())
    }

    #[inline]
    async unsafe fn write_vectored_inner(
        &mut self,
        stream: &mut (impl tokio::io::AsyncWrite + Unpin),
    ) -> std::io::Result<usize> {
        let mut bufs = self.inner.make_contiguous();
        if bufs.is_empty() {
            return Ok(0);
        }
        let orig_bufs_len = bufs.len();
        let n = stream.write_vectored(bufs).await?;
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        IoSlice::advance_slices(&mut bufs, n);
        let iovecs_to_remove = orig_bufs_len - bufs.len();
        self.inner.drain(..iovecs_to_remove);
        Ok(n)
    }

    #[inline]
    async unsafe fn flush_flattened_inner(
        &mut self,
        stream: &mut (impl tokio::io::AsyncWrite + Unpin),
    ) -> std::io::Result<()> {
        let mut buf = BytesMut::new();
        while let Some(ioslice) = self.inner.pop_front() {
            buf.extend_from_slice(unsafe {
                std::slice::from_raw_parts(ioslice.as_ptr(), ioslice.len())
            });
        }
        stream.write_all(&buf).await?;
        Ok(())
    }

    #[inline]
    async unsafe fn write_flattened_inner(
        &mut self,
        stream: &mut (impl tokio::io::AsyncWrite + Unpin),
    ) -> std::io::Result<usize> {
        let mut bufs = self.inner.make_contiguous();
        if bufs.is_empty() {
            return Ok(0);
        }
        let orig_bufs_len = bufs.len();
        let mut buf = BytesMut::new();
        for iovec in bufs.iter() {
            buf.extend_from_slice(unsafe {
                std::slice::from_raw_parts(iovec.as_ptr(), iovec.len())
            });
            if buf.len() >= 16384 {
                break;
            }
        }
        let n = stream.write(&buf).await?;
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        IoSlice::advance_slices(&mut bufs, n);
        let iovecs_to_remove = orig_bufs_len - bufs.len();
        self.inner.drain(..iovecs_to_remove);
        Ok(n)
    }

    #[inline]
    pub async unsafe fn write(
        &mut self,
        stream: &mut (impl tokio::io::AsyncWrite + Unpin),
        use_vectored: bool,
    ) -> std::io::Result<usize> {
        unsafe {
            if stream.is_write_vectored() && use_vectored {
                self.write_vectored_inner(stream).await
            } else {
                self.write_flattened_inner(stream).await
            }
        }
    }

    #[inline]
    pub async unsafe fn flush(
        &mut self,
        stream: &mut (impl tokio::io::AsyncWrite + Unpin),
        use_vectored: bool,
    ) -> std::io::Result<()> {
        unsafe {
            if stream.is_write_vectored() && use_vectored {
                self.flush_vectored_inner(stream).await
            } else {
                self.flush_flattened_inner(stream).await
            }
        }
    }
}
