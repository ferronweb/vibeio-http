const DEFAULT_CONN_WINDOW: u32 = 1024 * 1024;
const DEFAULT_STREAM_WINDOW: u32 = 1024 * 1024;
const DEFAULT_MAX_FRAME_SIZE: u32 = 1024 * 16;
const DEFAULT_MAX_SEND_BUF_SIZE: usize = 1024 * 400;
const DEFAULT_SETTINGS_MAX_HEADER_LIST_SIZE: u32 = 1024 * 16;

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

    pub fn h2_builder(&mut self) -> &mut h2::server::Builder {
        &mut self.h2
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
        let mut builder = h2::server::Builder::new();
        builder
            .initial_window_size(DEFAULT_STREAM_WINDOW)
            .initial_connection_window_size(DEFAULT_CONN_WINDOW)
            .max_frame_size(DEFAULT_MAX_FRAME_SIZE)
            .max_send_buffer_size(DEFAULT_MAX_SEND_BUF_SIZE)
            .max_header_list_size(DEFAULT_SETTINGS_MAX_HEADER_LIST_SIZE);
        Self::new(builder)
    }
}
