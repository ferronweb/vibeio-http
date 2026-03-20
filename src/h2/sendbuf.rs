#[repr(usize)]
pub(crate) enum SendBuf {
    Buf(bytes::Bytes),
    None,
}

impl bytes::Buf for SendBuf {
    #[inline]
    fn remaining(&self) -> usize {
        match self {
            Self::Buf(b) => b.remaining(),
            Self::None => 0,
        }
    }

    #[inline]
    fn chunk(&self) -> &[u8] {
        match self {
            Self::Buf(b) => b.chunk(),
            Self::None => &[],
        }
    }

    #[inline]
    fn advance(&mut self, cnt: usize) {
        match self {
            Self::Buf(b) => b.advance(cnt),
            Self::None => {}
        }
    }

    #[inline]
    fn chunks_vectored<'a>(&'a self, dst: &mut [std::io::IoSlice<'a>]) -> usize {
        match self {
            Self::Buf(b) => b.chunks_vectored(dst),
            Self::None => 0,
        }
    }
}
