use std::{collections::VecDeque, io::IoSlice};

use bytes::{Bytes, BytesMut};
use tokio::io::AsyncWriteExt;

const COPIED_CHUNK_CAPACITY: usize = 1024;
const FLATTEN_WRITE_LIMIT: usize = 16384;

pub(super) struct WriteBuf {
    slices: VecDeque<IoSlice<'static>>,
    owned_bytes: VecDeque<Option<Bytes>>,
    copied_chunks: Vec<BytesMut>,
    flattened: BytesMut,
    queued_bytes: usize,
}

impl WriteBuf {
    #[inline]
    pub fn new() -> Self {
        Self {
            slices: VecDeque::new(),
            owned_bytes: VecDeque::new(),
            copied_chunks: Vec::new(),
            flattened: BytesMut::new(),
            queued_bytes: 0,
        }
    }

    #[inline]
    pub unsafe fn push(&mut self, ioslice: IoSlice<'_>) {
        self.queued_bytes += ioslice.len();
        self.slices
            .push_back(unsafe { std::mem::transmute::<IoSlice<'_>, IoSlice<'static>>(ioslice) });
        self.owned_bytes.push_back(None);
    }

    #[inline]
    pub fn push_bytes(&mut self, bytes: Bytes) {
        if bytes.is_empty() {
            return;
        }
        self.queued_bytes += bytes.len();
        self.slices.push_back(unsafe {
            std::mem::transmute::<IoSlice<'_>, IoSlice<'static>>(IoSlice::new(&bytes))
        });
        self.owned_bytes.push_back(Some(bytes));
    }

    #[inline]
    pub fn push_copy(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let need_new_chunk = self
            .copied_chunks
            .last()
            .is_none_or(|chunk| chunk.capacity() - chunk.len() < bytes.len());
        if need_new_chunk {
            self.copied_chunks.push(BytesMut::with_capacity(
                bytes.len().max(COPIED_CHUNK_CAPACITY),
            ));
        }
        let chunk = self
            .copied_chunks
            .last_mut()
            .expect("copied chunk must exist");
        let start = chunk.len();
        chunk.extend_from_slice(bytes);
        self.queued_bytes += bytes.len();
        self.slices.push_back(unsafe {
            std::mem::transmute::<IoSlice<'_>, IoSlice<'static>>(IoSlice::new(&chunk[start..]))
        });
        self.owned_bytes.push_back(None);
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.queued_bytes
    }

    #[inline]
    fn fill_flattened(&mut self, limit: usize) -> usize {
        self.flattened.clear();
        let target = limit.min(self.queued_bytes);
        self.flattened.reserve(target);
        for ioslice in self.slices.iter() {
            let remaining = target - self.flattened.len();
            if remaining == 0 {
                break;
            }
            let slice = unsafe { std::slice::from_raw_parts(ioslice.as_ptr(), ioslice.len()) };
            let to_copy = slice.len().min(remaining);
            self.flattened.extend_from_slice(&slice[..to_copy]);
            if to_copy < slice.len() {
                break;
            }
        }
        self.flattened.len()
    }

    #[inline]
    fn consume_written(&mut self, written: usize) {
        self.queued_bytes = self.queued_bytes.saturating_sub(written);
        let consumed_slices = {
            let mut slices = self.slices.make_contiguous();
            let original_len = slices.len();
            IoSlice::advance_slices(&mut slices, written);
            original_len - slices.len()
        };
        self.slices.drain(..consumed_slices);
        self.owned_bytes.drain(..consumed_slices);
        if self.slices.is_empty() {
            self.queued_bytes = 0;
            self.owned_bytes.clear();
            self.flattened.clear();
            if self.copied_chunks.len() > 1 {
                let mut retained_chunk = self
                    .copied_chunks
                    .pop()
                    .expect("copied chunks is not empty");
                retained_chunk.clear();
                self.copied_chunks.clear();
                self.copied_chunks.push(retained_chunk);
            } else if let Some(chunk) = self.copied_chunks.first_mut() {
                chunk.clear();
            }
        }
    }

    #[inline]
    async unsafe fn flush_vectored_inner(
        &mut self,
        stream: &mut (impl tokio::io::AsyncWrite + Unpin),
    ) -> std::io::Result<()> {
        while !self.slices.is_empty() {
            let n = stream.write_vectored(self.slices.make_contiguous()).await?;
            if n == 0 {
                return Err(std::io::ErrorKind::WriteZero.into());
            }
            self.consume_written(n);
        }
        Ok(())
    }

    #[inline]
    async unsafe fn write_vectored_inner(
        &mut self,
        stream: &mut (impl tokio::io::AsyncWrite + Unpin),
    ) -> std::io::Result<usize> {
        if self.slices.is_empty() {
            return Ok(0);
        }
        let n = stream.write_vectored(self.slices.make_contiguous()).await?;
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        self.consume_written(n);
        Ok(n)
    }

    #[inline]
    async unsafe fn flush_flattened_inner(
        &mut self,
        stream: &mut (impl tokio::io::AsyncWrite + Unpin),
    ) -> std::io::Result<()> {
        while !self.slices.is_empty() {
            let len = self.fill_flattened(FLATTEN_WRITE_LIMIT);
            stream.write_all(&self.flattened[..len]).await?;
            self.consume_written(len);
        }
        Ok(())
    }

    #[inline]
    async unsafe fn write_flattened_inner(
        &mut self,
        stream: &mut (impl tokio::io::AsyncWrite + Unpin),
    ) -> std::io::Result<usize> {
        if self.slices.is_empty() {
            return Ok(0);
        }
        let len = self.fill_flattened(FLATTEN_WRITE_LIMIT);
        let n = stream.write(&self.flattened[..len]).await?;
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        self.consume_written(n);
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
