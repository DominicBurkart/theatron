use theatron::time::SimTime;
use theatron::traits::TrafficModel;

pub struct FileFragmenter {
    data: Vec<u8>,
    offset: usize,
    chunk_size: usize,
    interval_us: u64,
    next_send: SimTime,
}

impl FileFragmenter {
    pub fn new(data: Vec<u8>, chunk_size: usize, interval_us: u64) -> Self {
        Self {
            data,
            offset: 0,
            chunk_size,
            interval_us,
            next_send: 0,
        }
    }

    #[allow(dead_code)]
    pub fn is_done(&self) -> bool {
        self.offset >= self.data.len()
    }

    pub fn next_available_time(&self, current_time: SimTime) -> Option<SimTime> {
        if self.offset >= self.data.len() {
            None
        } else if current_time < self.next_send {
            Some(self.next_send)
        } else {
            Some(current_time)
        }
    }
}

impl TrafficModel for FileFragmenter {
    fn next_payload(&mut self, time: SimTime) -> Option<Vec<u8>> {
        if self.offset >= self.data.len() || time < self.next_send {
            return None;
        }
        let end = (self.offset + self.chunk_size).min(self.data.len());
        let chunk = self.data[self.offset..end].to_vec();
        self.offset = end;
        self.next_send = time + self.interval_us;
        Some(chunk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragments_data_in_chunks() {
        let data = vec![1u8, 2, 3, 4, 5];
        let mut f = FileFragmenter::new(data, 2, 0);
        assert_eq!(f.next_payload(0), Some(vec![1, 2]));
        assert_eq!(f.next_payload(0), Some(vec![3, 4]));
        assert_eq!(f.next_payload(0), Some(vec![5]));
        assert_eq!(f.next_payload(0), None);
    }

    #[test]
    fn respects_interval() {
        let data = vec![1u8, 2, 3, 4];
        let mut f = FileFragmenter::new(data, 2, 1_000);
        assert_eq!(f.next_payload(0), Some(vec![1, 2]));
        assert_eq!(f.next_payload(500), None);
        assert_eq!(f.next_payload(1_000), Some(vec![3, 4]));
    }

    /// `is_done` flips precisely when the final byte has been consumed —
    /// not before. This guards the contract `lorawan_adapter` relies on
    /// to stop scheduling wakes.
    #[test]
    fn is_done_only_after_full_consumption() {
        let mut f = FileFragmenter::new(vec![1u8, 2, 3], 2, 0);
        assert!(!f.is_done(), "fresh fragmenter is not done");
        f.next_payload(0).expect("first chunk available");
        assert!(
            !f.is_done(),
            "still has 1 byte remaining after first 2-byte chunk"
        );
        f.next_payload(0).expect("final byte available");
        assert!(f.is_done(), "all data consumed");
        // Subsequent calls remain done and return None.
        assert!(f.next_payload(0).is_none());
        assert!(f.is_done());
    }

    /// Empty input is "done" from creation and never produces a payload.
    /// `next_available_time` must report `None` because there is nothing
    /// to schedule.
    #[test]
    fn empty_data_is_done_from_creation() {
        let mut f = FileFragmenter::new(Vec::new(), 8, 1_000);
        assert!(f.is_done());
        assert_eq!(f.next_payload(0), None);
        assert_eq!(f.next_available_time(0), None);
        assert_eq!(f.next_available_time(u64::MAX), None);
    }

    // --- next_available_time: three documented branches ---

    /// Branch 1: fragmenter is exhausted → `None`.
    /// Once `is_done()`, callers must not be rescheduled.
    #[test]
    fn next_available_time_returns_none_when_done() {
        let mut f = FileFragmenter::new(vec![0xAB], 1, 1_000);
        f.next_payload(0).expect("consume only chunk");
        assert!(f.is_done());
        assert_eq!(f.next_available_time(0), None);
        assert_eq!(f.next_available_time(2_000), None);
    }

    /// Branch 2: data remains, but caller is too early → returns the
    /// earliest legal send time (`next_send`). This is what makes the
    /// adapter wake exactly at the interval boundary, not before.
    #[test]
    fn next_available_time_returns_next_send_when_early() {
        let mut f = FileFragmenter::new(vec![1u8, 2, 3, 4], 2, 5_000);
        // After consuming the first chunk at t=100, next_send becomes 5_100.
        f.next_payload(100).expect("first chunk");
        assert_eq!(
            f.next_available_time(100),
            Some(5_100),
            "between intervals, must return next_send"
        );
        assert_eq!(
            f.next_available_time(5_099),
            Some(5_100),
            "still too early one tick before next_send"
        );
    }

    /// Branch 3: data remains and we are at-or-past `next_send` → returns
    /// `current_time`, meaning "you may send immediately".
    #[test]
    fn next_available_time_returns_current_when_ready() {
        let mut f = FileFragmenter::new(vec![1u8, 2, 3, 4], 2, 5_000);
        f.next_payload(100).expect("first chunk");
        // At exactly next_send: ready now.
        assert_eq!(f.next_available_time(5_100), Some(5_100));
        // Past next_send: still "now".
        assert_eq!(f.next_available_time(9_999), Some(9_999));
    }

    /// `next_available_time` must be a pure observer: calling it does not
    /// advance internal state, so the next `next_payload` outcome is
    /// unaffected.
    #[test]
    fn next_available_time_does_not_mutate_state() {
        let mut f = FileFragmenter::new(vec![1u8, 2, 3, 4], 2, 1_000);
        f.next_payload(0).expect("first chunk");
        let _ = f.next_available_time(0);
        let _ = f.next_available_time(500);
        let _ = f.next_available_time(2_000);
        // Still gated by next_send=1_000.
        assert_eq!(f.next_payload(500), None);
        assert_eq!(f.next_payload(1_000), Some(vec![3, 4]));
    }

    /// A chunk_size larger than the data must yield exactly one payload
    /// containing the entire data, then exhaustion. This pins the
    /// `min(self.data.len())` clamp.
    #[test]
    fn chunk_size_larger_than_data_yields_single_payload() {
        let mut f = FileFragmenter::new(vec![1u8, 2, 3], 16, 0);
        assert_eq!(f.next_payload(0), Some(vec![1, 2, 3]));
        assert_eq!(f.next_payload(0), None);
        assert!(f.is_done());
    }

    /// Zero interval allows back-to-back sends at the same time. This is
    /// the configuration used by simulator scenarios that want maximum
    /// throughput limited only by airtime.
    #[test]
    fn zero_interval_allows_immediate_back_to_back() {
        let mut f = FileFragmenter::new(vec![1u8, 2, 3, 4], 2, 0);
        assert_eq!(f.next_payload(42), Some(vec![1, 2]));
        assert_eq!(
            f.next_available_time(42),
            Some(42),
            "zero interval keeps next_send <= time"
        );
        assert_eq!(f.next_payload(42), Some(vec![3, 4]));
        assert!(f.is_done());
    }

    /// Concatenating every emitted payload must reconstruct the original
    /// data byte-for-byte. This is the round-trip property the receiver
    /// (`NetworkServer`) relies on.
    #[test]
    fn concatenated_payloads_roundtrip_original_data() {
        let original: Vec<u8> = (0u8..50).collect();
        let mut f = FileFragmenter::new(original.clone(), 7, 0);
        let mut reassembled = Vec::new();
        while let Some(chunk) = f.next_payload(0) {
            assert!(chunk.len() <= 7, "chunk must not exceed chunk_size");
            assert!(!chunk.is_empty(), "chunk must be non-empty");
            reassembled.extend_from_slice(&chunk);
        }
        assert_eq!(reassembled, original);
        assert!(f.is_done());
    }
}
