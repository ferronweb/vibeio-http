pub struct Http3Options {
    pub(super) h3: h3::server::Builder,
    pub(super) accept_timeout: Option<std::time::Duration>,
    pub(super) handshake_timeout: Option<std::time::Duration>,
    pub(super) send_continue_response: bool,
}

impl Http3Options {
    pub fn new(h3: h3::server::Builder) -> Self {
        Self {
            h3,
            accept_timeout: Some(std::time::Duration::from_secs(30)),
            handshake_timeout: Some(std::time::Duration::from_secs(30)),
            send_continue_response: true,
        }
    }

    pub fn h3_builder(&mut self) -> &mut h3::server::Builder {
        &mut self.h3
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

impl Default for Http3Options {
    fn default() -> Self {
        Self::new(h3::server::builder())
    }
}
