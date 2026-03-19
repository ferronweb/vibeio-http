#[derive(Debug, Clone)]
pub struct Http2Options {
    pub(super) h2: h2::server::Builder,
    pub(super) accept_timeout: Option<std::time::Duration>,
    pub(super) handshake_timeout: Option<std::time::Duration>,
    pub(super) send_continue_response: bool,
}

impl Http2Options {
    pub fn new(h2: h2::server::Builder) -> Self {
        Self {
            h2,
            accept_timeout: Some(std::time::Duration::from_secs(30)),
            handshake_timeout: Some(std::time::Duration::from_secs(30)),
            send_continue_response: true,
        }
    }

    pub fn accept_timeout(mut self, timeout: Option<std::time::Duration>) -> Self {
        self.accept_timeout = timeout;
        self
    }

    pub fn handshake_timeout(mut self, timeout: Option<std::time::Duration>) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    pub fn send_continue_response(mut self, send: bool) -> Self {
        self.send_continue_response = send;
        self
    }
}

impl Default for Http2Options {
    fn default() -> Self {
        Self::new(h2::server::Builder::new())
    }
}
