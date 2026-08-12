use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use http::HeaderValue;

#[derive(Default)]
pub(crate) struct DateCache {
    inner: Mutex<Option<(String, std::time::SystemTime)>>,
}

impl DateCache {
    #[allow(dead_code)]
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn get_date_header_value(&self) -> Option<HeaderValue> {
        let now = std::time::SystemTime::now();
        let mut inner = self.inner.lock().ok()?;
        if inner.as_ref().is_none_or(|v| {
            v.1.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())
                != now.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())
        }) {
            let value = httpdate::fmt_http_date(now).to_string();
            inner.replace((value, now));
        }
        HeaderValue::from_str(inner.as_ref().map(|v| v.0.as_str()).unwrap_or("")).ok()
    }
}
