/// Configuration options for the HTTP/3 connection handler.
///
/// Use the builder-style methods to customise behaviour, then pass the finished
/// value to [`Http3::new`](super::Http3::new).
///
/// > **Note:** The underlying [`h3`] crate is still experimental. The API may
/// > change in future releases and there may be occasional bugs. Use with care
/// > in production environments.
///
/// # Examples
///
/// ```rust,ignore
/// let options = Http3Options::default()
///     .handshake_timeout(Some(std::time::Duration::from_secs(10)))
///     .accept_timeout(Some(std::time::Duration::from_secs(60)));
/// ```
pub struct Http3Options {
    pub(super) h3: h3::server::Builder,
    pub(super) accept_timeout: Option<std::time::Duration>,
    pub(super) handshake_timeout: Option<std::time::Duration>,
    pub(super) send_continue_response: bool,
    pub(super) send_date_header: bool,
}

impl Http3Options {
    /// Creates a new `Http3Options` from an `h3` server builder with the
    /// following defaults:
    ///
    /// | Option | Default |
    /// |---|---|
    /// | `accept_timeout` | 30 seconds |
    /// | `handshake_timeout` | 30 seconds |
    /// | `send_continue_response` | `true` |
    /// | `send_date_header` | `true` |
    ///
    /// The `h3` builder is used as-is and is not modified by this method.
    pub fn new(h3: h3::server::Builder) -> Self {
        Self {
            h3,
            accept_timeout: Some(std::time::Duration::from_secs(30)),
            handshake_timeout: Some(std::time::Duration::from_secs(30)),
            send_continue_response: true,
            send_date_header: true,
        }
    }

    /// Returns a mutable reference to the underlying `h3::server::Builder`.
    ///
    /// Use this to tune HTTP/3 protocol settings exposed by the [`h3`] crate.
    ///
    /// > **Note:** The [`h3`] crate is still experimental and its builder API
    /// > may change in future releases.
    pub fn h3_builder(&mut self) -> &mut h3::server::Builder {
        &mut self.h3
    }

    /// Sets the timeout for waiting on the next accepted HTTP/3 request
    /// resolver.
    ///
    /// If no new request arrives before this duration, the connection is
    /// gracefully shut down and the handler returns a timeout error.
    /// Pass `None` to disable this timeout. Defaults to **30 seconds**.
    pub fn accept_timeout(mut self, timeout: Option<std::time::Duration>) -> Self {
        self.accept_timeout = timeout;
        self
    }

    /// Sets the timeout for the initial HTTP/3 connection setup (QUIC
    /// handshake and `h3` connection build).
    ///
    /// If the setup does not complete within this duration, the handler
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

    /// Controls whether a `Date` header is automatically added to every
    /// response.
    ///
    /// The value is cached and refreshed at most once per second.
    /// Defaults to **`true`**.
    pub fn send_date_header(mut self, send: bool) -> Self {
        self.send_date_header = send;
        self
    }
}

impl Default for Http3Options {
    fn default() -> Self {
        Self::new(h3::server::builder())
    }
}
