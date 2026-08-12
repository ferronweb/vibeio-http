use std::time::Duration;

use crate::h2::codec::{DEFAULT_INITIAL_WINDOW_SIZE, DEFAULT_MAX_FRAME_SIZE};

/// HTTP/2 server configuration.
///
/// Build one with [`Http2Options::default`] and override individual fields
/// with the builder methods, then hand it to [`Http2::new`](crate::Http2::new).
///
/// Unlike the framing/header limits below, the connection window settings
/// ([`initial_stream_window_size`](Http2Options::initial_stream_window_size)
/// and [`initial_connection_window_size`](Http2Options::initial_connection_window_size))
/// are advisory: a client may shrink them with its own `SETTINGS`, and the
/// server honours the smaller value.
#[derive(Debug, Clone)]
pub struct Http2Options {
    /// Max time to wait for the client's preface before giving up.
    pub(crate) handshake_timeout: Option<Duration>,
    /// Send a `100 Continue` response as soon as a request's headers arrive,
    /// before its body has been fully read.
    pub(crate) send_continue_response: bool,
    /// Insert a `Date` header into every response when absent.
    pub(crate) send_date_header: bool,
    /// Maximum number of concurrent streams the server allows.
    pub(crate) max_concurrent_streams: u32,
    /// Initial per-stream flow-control window the server advertises.
    pub(crate) initial_stream_window_size: u32,
    /// Initial connection-level flow-control window the server uses.
    pub(crate) initial_connection_window_size: u32,
    /// Largest frame payload the server will send or receive.
    pub(crate) max_frame_size: u32,
    /// Largest uncompressed header list the server will accept.
    pub(crate) max_header_list_size: u32,
}

impl Default for Http2Options {
    #[inline]
    fn default() -> Self {
        Http2Options {
            handshake_timeout: Some(Duration::from_secs(10)),
            send_continue_response: true,
            send_date_header: true,
            max_concurrent_streams: 200,
            initial_stream_window_size: DEFAULT_INITIAL_WINDOW_SIZE,
            initial_connection_window_size: DEFAULT_INITIAL_WINDOW_SIZE,
            max_frame_size: DEFAULT_MAX_FRAME_SIZE as u32,
            max_header_list_size: u32::MAX,
        }
    }
}

impl Http2Options {
    /// Sets the maximum time to wait for a client to send the HTTP/2
    /// preface before aborting the connection.
    #[inline]
    pub fn handshake_timeout(mut self, handshake_timeout: Option<Duration>) -> Self {
        self.handshake_timeout = handshake_timeout;
        self
    }

    /// Sends `100 Continue` responses automatically when a request has a body.
    ///
    /// Defaults to `true`.
    #[inline]
    pub fn send_continue_response(mut self, send_continue_response: bool) -> Self {
        self.send_continue_response = send_continue_response;
        self
    }

    /// Inserts a `Date` header into responses that lack one.
    ///
    /// Defaults to `true`.
    #[inline]
    pub fn send_date_header(mut self, send_date_header: bool) -> Self {
        self.send_date_header = send_date_header;
        self
    }

    /// Sets the maximum number of concurrent streams allowed on a connection.
    ///
    /// Defaults to `200`.
    #[inline]
    pub fn max_concurrent_streams(mut self, max_concurrent_streams: u32) -> Self {
        self.max_concurrent_streams = max_concurrent_streams;
        self
    }

    /// Sets the initial per-stream flow-control window size advertised to the
    /// client. Defaults to the RFC 9113 default (`65_535`).
    #[inline]
    pub fn initial_stream_window_size(mut self, initial_stream_window_size: u32) -> Self {
        self.initial_stream_window_size = initial_stream_window_size;
        self
    }

    /// Sets the initial connection-level flow-control window size.
    /// Defaults to the RFC 9113 default (`65_535`).
    #[inline]
    pub fn initial_connection_window_size(mut self, initial_connection_window_size: u32) -> Self {
        self.initial_connection_window_size = initial_connection_window_size;
        self
    }

    /// Sets the maximum frame size the server will send or receive.
    /// Defaults to the RFC 9113 default (`16_384`); must not exceed
    /// `2^24 - 1`.
    #[inline]
    pub fn max_frame_size(mut self, max_frame_size: u32) -> Self {
        self.max_frame_size = max_frame_size;
        self
    }

    /// Sets the maximum size of an uncompressed header list the server will
    /// accept. Defaults to `u32::MAX` (unbounded).
    #[inline]
    pub fn max_header_list_size(mut self, max_header_list_size: u32) -> Self {
        self.max_header_list_size = max_header_list_size;
        self
    }
}
