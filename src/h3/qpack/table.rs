//! QPACK dynamic table (RFC 9204 Section 3.2).
//!
//! The dynamic table is a FIFO list of field lines shared between the
//! encoder and the decoder. Entries are added at the insertion point and
//! evicted from the dropping point (oldest first) to keep the table size
//! within its capacity. The size of an entry is the sum of its name length,
//! its value length, and 32 additional bytes (Section 3.2.1).
//!
//! Absolute indices are fixed for the lifetime of an entry; relative and
//! post-base indices are computed from the context (most-recent insertion
//! for encoder-stream instructions, the field section's Base for field line
//! representations). The table itself is context-free: callers translate
//! indices via the lookup helpers.
//!
//! The dynamic table can contain duplicate entries, and entries can have
//! empty values; neither is an error (Section 3.2).
//!
//! Consumption: the encoder adds entries (Section 4.3) and the decoder
//! materializes them from encoder-stream instructions; both use the same
//! structure. The lookup helpers are first used by the encoder, which is
//! why `dead_code` fires until then; it errors again once they are used,
//! reminding us to remove the expectation.
#![expect(dead_code)]

use std::collections::VecDeque;

use bytes::Bytes;

/// Error returned when an entry cannot be inserted.
///
/// The caller maps this to `QPACK_ENCODER_STREAM_ERROR` on the decoder
/// side; a well-behaved encoder never triggers it (it only inserts entries
/// that fit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InsertError {
    /// The entry (name + value + 32) is larger than the table capacity.
    EntryTooLarge,
}

/// FIFO dynamic table. `entries[0]` is the most recently inserted entry.
///
/// Invariants:
/// - `size <= capacity` at all times;
/// - the absolute index of `entries[i]` is `inserted - 1 - i`;
/// - `inserted` counts entries inserted over the table's whole lifetime
///   (absolute indices are never reused).
pub(crate) struct DynamicTable {
    entries: VecDeque<(Bytes, Bytes)>,
    capacity: u64,
    size: u64,
    inserted: u64,
}

impl DynamicTable {
    /// Creates an empty table with the given initial `capacity`.
    #[inline]
    pub(crate) fn new(capacity: u64) -> Self {
        Self {
            entries: VecDeque::new(),
            capacity,
            size: 0,
            inserted: 0,
        }
    }

    /// The current dynamic table capacity.
    #[inline]
    pub(crate) fn capacity(&self) -> u64 {
        self.capacity
    }

    /// The current sum of entry sizes.
    #[inline]
    pub(crate) fn size(&self) -> u64 {
        self.size
    }

    /// Number of entries currently in the table.
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the table is empty.
    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Absolute index of the most recently inserted entry, or `0` when the
    /// table is empty.
    #[inline]
    pub(crate) fn last_absolute(&self) -> u64 {
        self.inserted.saturating_sub(1)
    }

    /// The number of entries inserted over the table's lifetime.
    #[inline]
    pub(crate) fn inserted(&self) -> u64 {
        self.inserted
    }

    /// The absolute index a newly inserted entry will receive.
    #[inline]
    pub(crate) fn next_absolute(&self) -> u64 {
        self.inserted
    }

    /// The size contribution of an entry: name + value + 32 (RFC 9204
    /// Section 3.2.1).
    #[inline]
    pub(crate) fn entry_size(name: &[u8], value: &[u8]) -> u64 {
        name.len() as u64 + value.len() as u64 + 32
    }

    /// Changes the table capacity, evicting entries from the dropping point
    /// (oldest first) until the table fits.
    ///
    /// Setting the capacity to 0 clears the table; a later increase restores
    /// normal operation with an empty table (RFC 9204 Section 3.2.2).
    #[inline]
    pub(crate) fn set_capacity(&mut self, capacity: u64) {
        self.capacity = capacity;
        self.evict_to_fit(capacity);
    }

    /// Inserts a new entry at the insertion point, evicting oldest entries
    /// as needed.
    ///
    /// Returns [`InsertError::EntryTooLarge`] if the entry does not fit in
    /// the table capacity at all; on success the entry receives absolute
    /// index `inserted()`.
    pub(crate) fn insert(&mut self, name: Bytes, value: Bytes) -> Result<(), InsertError> {
        let entry_size = Self::entry_size(&name, &value);
        if entry_size > self.capacity {
            return Err(InsertError::EntryTooLarge);
        }
        self.evict_to_fit(self.capacity - entry_size);
        self.entries.push_front((name, value));
        self.size += entry_size;
        self.inserted += 1;
        Ok(())
    }

    /// Returns the (name, value) pair with the given absolute index.
    #[inline]
    pub(crate) fn get_absolute(&self, abs: u64) -> Option<(&[u8], &[u8])> {
        let last = self.last_absolute();
        if abs > last {
            return None;
        }
        self.entry_at(last - abs)
    }

    /// Returns the (name, value) pair referenced by an encoder-stream
    /// relative index: 0 is the most recently inserted entry.
    #[inline]
    pub(crate) fn get_relative(&self, index: u64) -> Option<(&[u8], &[u8])> {
        self.entry_at(index)
    }

    /// Returns the (name, value) pair referenced by a field line
    /// representation relative index: index 0 is the entry with absolute
    /// index `base - 1`.
    #[inline]
    pub(crate) fn get_base_relative(&self, base: u64, index: u64) -> Option<(&[u8], &[u8])> {
        if index >= base {
            return None;
        }
        let abs = base - 1 - index;
        self.get_absolute(abs)
    }

    /// Returns the (name, value) pair referenced by a post-base index:
    /// index 0 is the entry with absolute index `base`.
    #[inline]
    pub(crate) fn get_post_base(&self, base: u64, index: u64) -> Option<(&[u8], &[u8])> {
        base.checked_add(index)
            .and_then(|abs| self.get_absolute(abs))
    }

    /// Returns the (name, value) pair at deque position `i` (0 = most
    /// recently inserted).
    #[inline]
    fn entry_at(&self, i: u64) -> Option<(&[u8], &[u8])> {
        let i = usize::try_from(i).ok()?;
        let (name, value) = self.entries.get(i)?;
        Some((name.as_ref(), value.as_ref()))
    }

    /// Evicts entries from the dropping point until `size <= max_size`.
    #[inline]
    fn evict_to_fit(&mut self, max_size: u64) {
        while self.size > max_size {
            let (name, value) = match self.entries.pop_back() {
                Some(entry) => entry,
                None => break,
            };
            self.size -= Self::entry_size(&name, &value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert(table: &mut DynamicTable, name: &str, value: &str) -> Result<(), InsertError> {
        table.insert(
            Bytes::copy_from_slice(name.as_bytes()),
            Bytes::copy_from_slice(value.as_bytes()),
        )
    }

    #[test]
    fn entry_size_is_name_plus_value_plus_32() {
        assert_eq!(DynamicTable::entry_size(b"foo", b"bar"), 38);
        assert_eq!(DynamicTable::entry_size(b"", b""), 32);
    }

    #[test]
    fn insert_assigns_increasing_absolute_indices() {
        let mut table = DynamicTable::new(1000);
        assert!(table.is_empty());
        assert_eq!(table.inserted(), 0);
        assert_eq!(table.next_absolute(), 0);

        insert(&mut table, ":method", "GET").unwrap();
        assert_eq!(table.inserted(), 1);
        assert_eq!(table.last_absolute(), 0);

        insert(&mut table, ":path", "/").unwrap();
        assert_eq!(table.inserted(), 2);
        assert_eq!(table.last_absolute(), 1);
        assert_eq!(table.get_absolute(0), Some((&b":method"[..], &b"GET"[..])));
        assert_eq!(table.get_absolute(1), Some((&b":path"[..], &b"/"[..])));
        assert_eq!(table.get_absolute(2), None);
    }

    #[test]
    fn relative_index_zero_is_most_recent() {
        let mut table = DynamicTable::new(1000);
        insert(&mut table, "a", "1").unwrap();
        insert(&mut table, "b", "2").unwrap();
        insert(&mut table, "c", "3").unwrap();

        assert_eq!(table.get_relative(0), Some((&b"c"[..], &b"3"[..])));
        assert_eq!(table.get_relative(1), Some((&b"b"[..], &b"2"[..])));
        assert_eq!(table.get_relative(2), Some((&b"a"[..], &b"1"[..])));
        assert_eq!(table.get_relative(3), None);
    }

    #[test]
    fn insert_evicts_oldest_first() {
        // capacity 100: entry size 1+1+32 = 34; three entries = 102 > 100,
        // so inserting the third evicts the first.
        let mut table = DynamicTable::new(100);
        insert(&mut table, "a", "a").unwrap();
        insert(&mut table, "b", "b").unwrap();
        assert_eq!(table.size(), 68);
        assert_eq!(table.len(), 2);

        insert(&mut table, "c", "c").unwrap();
        assert_eq!(table.size(), 68);
        assert_eq!(table.len(), 2);
        assert_eq!(table.get_absolute(0), None, "oldest entry evicted");
        assert_eq!(table.get_absolute(1), Some((&b"b"[..], &b"b"[..])));
        assert_eq!(table.get_absolute(2), Some((&b"c"[..], &b"c"[..])));
        // Absolute indices of evicted entries are never reused.
        assert_eq!(table.inserted(), 3);
    }

    #[test]
    fn insert_rejects_oversized_entry() {
        let mut table = DynamicTable::new(10);
        assert_eq!(
            insert(&mut table, "a", "a"),
            Err(InsertError::EntryTooLarge)
        );
        assert!(table.is_empty());
        assert_eq!(table.inserted(), 0);
    }

    #[test]
    fn set_capacity_evicts_and_can_clear() {
        let mut table = DynamicTable::new(1000);
        for i in 0..5 {
            insert(&mut table, &format!("h{i}"), "v").unwrap();
        }
        assert_eq!(table.len(), 5);

        // Shrink below the size of the two newest entries.
        table.set_capacity(80);
        assert!(table.size() <= 80);
        assert_eq!(table.len(), 2);

        // Setting 0 clears the table; a later increase works with an empty
        // table.
        table.set_capacity(0);
        assert!(table.is_empty());
        assert_eq!(table.size(), 0);
        table.set_capacity(1000);
        insert(&mut table, "fresh", "entry").unwrap();
        assert_eq!(table.len(), 1);
        assert_eq!(table.get_absolute(5), Some((&b"fresh"[..], &b"entry"[..])));
    }

    #[test]
    fn field_section_relative_and_post_base_indexing() {
        // Recreates the RFC 9204 Figure 3/4 scenario: 10 insertions, 3
        // evictions (alive absolute indices 3..=9), Base = 8.
        let mut table = DynamicTable::new(1000);
        for i in 0..10u8 {
            let bytes = Bytes::copy_from_slice(&[b'a' + i]);
            table.insert(bytes.clone(), bytes).unwrap();
        }
        // Evict the three oldest (absolute 0..=2) by shrinking capacity and
        // restoring it: 7 entries x 34 bytes each.
        table.set_capacity(7 * 34);
        table.set_capacity(1000);
        assert_eq!(table.len(), 7);
        assert_eq!(table.last_absolute(), 9);

        let base = 8;
        // Relative: index 0 -> absolute 7, increasing indices go older.
        assert_eq!(table.get_base_relative(base, 0), table.get_absolute(7));
        assert_eq!(table.get_base_relative(base, 3), table.get_absolute(4));
        assert_eq!(table.get_base_relative(base, 5), table.get_absolute(2));
        assert_eq!(table.get_base_relative(base, 5), None, "evicted entry");
        assert_eq!(table.get_base_relative(base, 8), None, "index >= base");

        // Post-base: index 0 -> absolute 8, increasing indices go newer.
        assert_eq!(table.get_post_base(base, 0), table.get_absolute(8));
        assert_eq!(table.get_post_base(base, 1), table.get_absolute(9));
        assert_eq!(table.get_post_base(base, 2), None, "beyond newest entry");
    }

    #[test]
    fn duplicate_entries_are_allowed() {
        let mut table = DynamicTable::new(1000);
        insert(&mut table, "cookie", "a=b").unwrap();
        insert(&mut table, "cookie", "a=b").unwrap();
        assert_eq!(table.len(), 2);
        assert_eq!(table.get_absolute(0), Some((&b"cookie"[..], &b"a=b"[..])));
        assert_eq!(table.get_absolute(1), Some((&b"cookie"[..], &b"a=b"[..])));
    }

    #[test]
    fn empty_table_lookups_return_none() {
        let table = DynamicTable::new(1000);
        assert_eq!(table.get_absolute(0), None);
        assert_eq!(table.get_relative(0), None);
        assert_eq!(table.get_base_relative(0, 0), None);
        assert_eq!(table.get_post_base(0, 0), None);
    }
}
