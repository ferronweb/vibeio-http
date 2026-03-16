#[derive(Debug, Clone)]
pub struct Http1Options {
    pub(crate) max_header_size: usize,
    pub(crate) max_header_count: usize,
    pub(crate) send_date_header: bool,
}

impl Http1Options {
    /// Creates a new `Http1Options` builder with default values.
    pub fn new() -> Self {
        Self {
            max_header_size: 16384,
            max_header_count: 128,
            send_date_header: true,
        }
    }

    pub fn max_header_size(mut self, size: usize) -> Self {
        self.max_header_size = size;
        self
    }

    pub fn max_header_count(mut self, count: usize) -> Self {
        self.max_header_count = count;
        self
    }

    pub fn send_date_header(mut self, send: bool) -> Self {
        self.send_date_header = send;
        self
    }
}

impl Default for Http1Options {
    fn default() -> Self {
        Self::new()
    }
}
