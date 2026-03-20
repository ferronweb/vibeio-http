const DEFAULT_CONN_WINDOW: u32 = 1024 * 1024;
const DEFAULT_STREAM_WINDOW: u32 = 1024 * 1024;
const DEFAULT_MAX_FRAME_SIZE: u32 = 1024 * 16;
const DEFAULT_MAX_SEND_BUF_SIZE: usize = 1024 * 400;
const DEFAULT_SETTINGS_MAX_HEADER_LIST_SIZE: u32 = 1024 * 16;
const DEFAULT_MAX_CONCURRENT_STREAMS: u32 = 200;

/// Configuration options for the HTTP/2 connection handler.
///
/// Use the builder-style methods to customise behaviour, then pass the finished
/// value to [`Http2::new`](super::Http2::new).
///
/// # Examples
///
/// ```rust,ignore
/// let options = Http2Options::default()
///     .handshake_timeout(Some(std::time::Duration::from_secs(10)))
///     .accept_timeout(Some(std::time::Duration::from_secs(60)));
/// ```
#[derive(Debug, Clone)]
pub struct Http2Options {
    pub(super) h2: h2::server::Builder,
    pub(super) accept_timeout: Option<std::time::Duration>,
    pub(super) handshake_timeout: Option<std::time::Duration>,
    pub(super) send_continue_response: bool,
}

impl Http2Options {
    /// Creates a new `Http2Options` from an `h2` server builder with the
    /// following defaults:
    ///
    /// | Option | Default |
    /// |---|---|
    /// | `accept_timeout` | 30 seconds |
    /// | `handshake_timeout` | 30 seconds |
    /// | `send_continue_response` | `true` |
    ///
    /// The `h2` builder is used as-is and is not modified by this method.
    pub fn new(h2: h2::server::Builder) -> Self {
        Self {
            h2,
            accept_timeout: Some(std::time::Duration::from_secs(30)),
            handshake_timeout: Some(std::time::Duration::from_secs(30)),
            send_continue_response: true,
        }
    }

    /// Returns a mutable reference to the underlying `h2::server::Builder`.
    ///
    /// Use this to tune HTTP/2 protocol settings such as flow-control windows,
    /// frame size limits, and header list size.
    pub fn h2_builder(&mut self) -> &mut h2::server::Builder {
        &mut self.h2
    }

    /// Sets the timeout for waiting on the next accepted HTTP/2 stream.
    ///
    /// If no new stream arrives before this duration, the connection is
    /// gracefully shut down and the handler returns a timeout error.
    /// Pass `None` to disable this timeout. Defaults to **30 seconds**.
    pub fn accept_timeout(mut self, timeout: Option<std::time::Duration>) -> Self {
        self.accept_timeout = timeout;
        self
    }

    /// Sets the timeout for the initial HTTP/2 handshake.
    ///
    /// If the handshake does not complete within this duration, the handler
    /// returns an I/O timeout error. Pass `None` to disable this timeout.
    /// Defaults to **30 seconds**.
    pub fn handshake_timeout(mut self, timeout: Option<std::time::Duration>) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    /// Controls whether a `100 Continue` interim response is sent when a
    /// request contains an `Expect: 100-continue` header.
    ///
    /// Defaults to **`true`**.
    pub fn send_continue_response(mut self, send: bool) -> Self {
        self.send_continue_response = send;
        self
    }
}

impl Default for Http2Options {
    fn default() -> Self {
        let mut builder = h2::server::Builder::new();
        builder
            .initial_window_size(DEFAULT_STREAM_WINDOW)
            .initial_connection_window_size(DEFAULT_CONN_WINDOW)
            .max_frame_size(DEFAULT_MAX_FRAME_SIZE)
            .max_send_buffer_size(DEFAULT_MAX_SEND_BUF_SIZE)
            .max_header_list_size(DEFAULT_SETTINGS_MAX_HEADER_LIST_SIZE)
            .max_concurrent_streams(DEFAULT_MAX_CONCURRENT_STREAMS);
        Self::new(builder)
    }
}
