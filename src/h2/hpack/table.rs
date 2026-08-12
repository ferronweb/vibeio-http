//! HPACK header tables (RFC 7541 Sections 2.3 and 4).

use bytes::Bytes;
use std::collections::VecDeque;

/// The static table (RFC 7541 Appendix A): 61 immutable entries.
/// Index 1 is `:authority`, index 61 is `www-authenticate`.
const STATIC_TABLE: [(&[u8], &[u8]); 61] = [
    (b":authority", b""),
    (b":method", b"GET"),
    (b":method", b"POST"),
    (b":path", b"/"),
    (b":path", b"/index.html"),
    (b":scheme", b"http"),
    (b":scheme", b"https"),
    (b":status", b"200"),
    (b":status", b"204"),
    (b":status", b"206"),
    (b":status", b"304"),
    (b":status", b"400"),
    (b":status", b"404"),
    (b":status", b"500"),
    (b"accept-charset", b""),
    (b"accept-encoding", b"gzip, deflate"),
    (b"accept-language", b""),
    (b"accept-ranges", b""),
    (b"accept", b""),
    (b"access-control-allow-origin", b""),
    (b"age", b""),
    (b"allow", b""),
    (b"authorization", b""),
    (b"cache-control", b""),
    (b"content-disposition", b""),
    (b"content-encoding", b""),
    (b"content-language", b""),
    (b"content-length", b""),
    (b"content-location", b""),
    (b"content-range", b""),
    (b"content-type", b""),
    (b"cookie", b""),
    (b"date", b""),
    (b"etag", b""),
    (b"expect", b""),
    (b"expires", b""),
    (b"from", b""),
    (b"host", b""),
    (b"if-match", b""),
    (b"if-modified-since", b""),
    (b"if-none-match", b""),
    (b"if-range", b""),
    (b"if-unmodified-since", b""),
    (b"last-modified", b""),
    (b"link", b""),
    (b"location", b""),
    (b"max-forwards", b""),
    (b"proxy-authenticate", b""),
    (b"proxy-authorization", b""),
    (b"range", b""),
    (b"referer", b""),
    (b"refresh", b""),
    (b"retry-after", b""),
    (b"server", b""),
    (b"set-cookie", b""),
    (b"strict-transport-security", b""),
    (b"transfer-encoding", b""),
    (b"user-agent", b""),
    (b"vary", b""),
    (b"via", b""),
    (b"www-authenticate", b""),
];

/// Number of entries in the static table.
pub(crate) const STATIC_LEN: usize = 61;

/// The entry-size overhead (RFC 7541 Section 4.1).
const ENTRY_OVERHEAD: usize = 32;

/// A header field name/value pair stored in or fetched from a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    name: Bytes,
    value: Bytes,
}

impl Header {
    pub(crate) fn new(name: Bytes, value: Bytes) -> Self {
        Header { name, value }
    }

    /// Size in octets as defined in RFC 7541 Section 4.1: the sum of the
    /// name and value lengths (without Huffman encoding) plus 32.
    pub(crate) fn size(&self) -> usize {
        ENTRY_OVERHEAD + self.name.len() + self.value.len()
    }

    pub fn name(&self) -> &[u8] {
        &self.name
    }

    pub fn value(&self) -> &[u8] {
        &self.value
    }
}

/// The HPACK header table: the immutable static table followed by a
/// dynamically sized FIFO (RFC 7541 Section 4).
#[derive(Debug)]
pub(crate) struct Table {
    /// Dynamic entries, newest at the front. The entry at index 62 in the
    /// combined addressing scheme is `entries[0]`.
    entries: VecDeque<Header>,
    /// Current combined size of the dynamic entries.
    size: usize,
    /// Maximum combined size the dynamic table may grow to.
    max_size: usize,
}

impl Table {
    pub(crate) fn new() -> Self {
        Table::with_max_size(DEFAULT_MAX_SIZE)
    }

    /// The default dynamic-table size when no SETTINGS_HEADER_TABLE_SIZE
    /// has been exchanged (RFC 7541 Section 4.2).
    pub(crate) fn with_max_size(max_size: usize) -> Self {
        Table {
            entries: VecDeque::new(),
            size: 0,
            max_size,
        }
    }

    /// Returns the entry at 1-based `index`: 1..=61 addresses the static
    /// table, 62.. the dynamic table (newest first).
    pub(crate) fn get(&self, index: usize) -> Option<Header> {
        if index == 0 {
            return None;
        }
        if index <= STATIC_LEN {
            let (name, value) = STATIC_TABLE[index - 1];
            // `from_static` is zero-copy.
            return Some(Header::new(
                Bytes::from_static(name),
                Bytes::from_static(value),
            ));
        }
        self.entries.get(index - STATIC_LEN - 1).cloned()
    }

    /// Number of dynamic entries.
    pub(crate) fn dynamic_len(&self) -> usize {
        self.entries.len()
    }

    /// Number of static and dynamic entries combined.
    pub(crate) fn len(&self) -> usize {
        STATIC_LEN + self.entries.len()
    }

    /// Current combined size of the dynamic entries.
    pub(crate) fn size(&self) -> usize {
        self.size
    }

    pub(crate) fn max_size(&self) -> usize {
        self.max_size
    }

    /// Changes the maximum table size, evicting entries from the end of
    /// the dynamic table until its size is within the new limit
    /// (RFC 7541 Section 4.3).
    pub(crate) fn set_max_size(&mut self, max_size: usize) {
        self.max_size = max_size;
        while self.size > self.max_size {
            match self.entries.pop_back() {
                Some(entry) => self.size -= entry.size(),
                None => break,
            }
        }
    }

    /// Adds an entry to the front of the dynamic table, evicting entries
    /// from the end as needed (RFC 7541 Section 4.4). An entry larger
    /// than the maximum size empties the table and is not added.
    pub(crate) fn add(&mut self, header: Header) {
        if header.size() > self.max_size {
            self.entries.clear();
            self.size = 0;
            return;
        }
        while self.size + header.size() > self.max_size {
            match self.entries.pop_back() {
                Some(entry) => self.size -= entry.size(),
                None => break,
            }
        }
        self.size += header.size();
        self.entries.push_front(header);
    }
}

impl Default for Table {
    fn default() -> Self {
        Self::new()
    }
}

/// RFC 7541 Section 4.2: the initial maximum dynamic table size is 4096
/// octets.
const DEFAULT_MAX_SIZE: usize = 4096;

#[cfg(test)]
mod tests {
    use super::*;

    fn header(name: &str, value: &str) -> Header {
        Header::new(
            Bytes::copy_from_slice(name.as_bytes()),
            Bytes::copy_from_slice(value.as_bytes()),
        )
    }

    #[test]
    fn static_table_contents() {
        assert_eq!(
            Table::new().get(1),
            Some(Header::new(
                Bytes::from_static(b":authority"),
                Bytes::from_static(b"")
            ))
        );
        assert_eq!(
            Table::new().get(2),
            Some(Header::new(
                Bytes::from_static(b":method"),
                Bytes::from_static(b"GET")
            ))
        );
        assert_eq!(
            Table::new().get(16),
            Some(Header::new(
                Bytes::from_static(b"accept-encoding"),
                Bytes::from_static(b"gzip, deflate")
            ))
        );
        assert_eq!(
            Table::new().get(61),
            Some(Header::new(
                Bytes::from_static(b"www-authenticate"),
                Bytes::from_static(b"")
            ))
        );
        assert_eq!(Table::new().get(0), None);
    }

    #[test]
    fn entry_size_math() {
        // 32 overhead + 4 name + 3 value.
        assert_eq!(header("test", "abc").size(), 39);
        assert_eq!(header("", "").size(), 32);
    }

    #[test]
    fn add_and_fetch_order() {
        let mut table = Table::with_max_size(200);
        table.add(header("a", "1"));
        table.add(header("b", "2"));
        table.add(header("c", "3"));

        // Newest entry is at index 62.
        assert_eq!(table.get(62), Some(header("c", "3")));
        assert_eq!(table.get(63), Some(header("b", "2")));
        assert_eq!(table.get(64), Some(header("a", "1")));
        assert_eq!(table.get(65), None);
        assert_eq!(table.dynamic_len(), 3);
        assert_eq!(table.len(), STATIC_LEN + 3);
        assert_eq!(table.size(), 34 * 3);
    }

    #[test]
    fn eviction_from_end() {
        // Each entry is 34 octets; only two fit in 70.
        let mut table = Table::with_max_size(70);
        table.add(header("a", "1"));
        table.add(header("b", "2"));
        table.add(header("c", "3"));

        assert_eq!(table.dynamic_len(), 2);
        assert_eq!(table.get(62), Some(header("c", "3")));
        assert_eq!(table.get(63), Some(header("b", "2")));
        assert_eq!(table.get(64), None);
        assert_eq!(table.size(), 68);
    }

    #[test]
    fn entry_larger_than_max_empties_table() {
        let mut table = Table::with_max_size(100);
        table.add(header("x", "1"));
        table.add(header("y", "2"));
        assert_eq!(table.dynamic_len(), 2);

        // 101 octets > 100 max.
        let big = header(&"v".repeat(60), &"v".repeat(9));
        assert_eq!(big.size(), 101);
        table.add(big);

        assert_eq!(table.dynamic_len(), 0);
        assert_eq!(table.size(), 0);
    }

    #[test]
    fn entry_exactly_max_size_is_added() {
        let mut table = Table::with_max_size(101);
        let big = header(&"v".repeat(60), &"v".repeat(9));
        assert_eq!(big.size(), 101);
        table.add(big);
        assert_eq!(table.dynamic_len(), 1);
    }

    #[test]
    fn size_update_evicts() {
        let mut table = Table::with_max_size(200);
        table.add(header("a", "1"));
        table.add(header("b", "2"));
        table.add(header("c", "3"));

        table.set_max_size(70);
        assert_eq!(table.dynamic_len(), 2);
        assert_eq!(table.get(62), Some(header("c", "3")));
        assert_eq!(table.size(), 68);

        table.set_max_size(0);
        assert_eq!(table.dynamic_len(), 0);
        assert_eq!(table.size(), 0);

        // Growing back to the original size keeps the table empty; new
        // entries populate it again.
        table.set_max_size(200);
        assert_eq!(table.dynamic_len(), 0);
        table.add(header("d", "4"));
        assert_eq!(table.get(62), Some(header("d", "4")));
    }

    #[test]
    fn max_size_tracks_requests() {
        let mut table = Table::with_max_size(128);
        assert_eq!(table.max_size(), 128);
        table.set_max_size(256);
        assert_eq!(table.max_size(), 256);
    }

    #[test]
    fn default_max_size() {
        assert_eq!(Table::new().max_size(), DEFAULT_MAX_SIZE);
        assert_eq!(Table::default().max_size(), DEFAULT_MAX_SIZE);
    }

    #[test]
    fn grows_and_reuses_slots() {
        // The FIFO must keep working after many evictions (the ring
        // reuses its backing slots).
        let mut table = Table::with_max_size(200);
        for i in 0..100 {
            let entry = header(&format!("k{i}"), "v");
            table.add(entry.clone());
            assert!(table.size() <= table.max_size(), "iteration {i}");
            assert_eq!(table.get(62), Some(entry), "iteration {i}");
        }
        assert_eq!(table.get(62), Some(header("k99", "v")));
        assert_eq!(
            table.size(),
            table.dynamic_len() * table.get(62).unwrap().size()
        );
    }
}
