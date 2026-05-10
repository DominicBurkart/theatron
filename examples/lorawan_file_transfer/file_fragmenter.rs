//! File fragmentation traffic model.
//!
//! `FileFragmenter` is the [`TrafficModel`] used by the LoRaWAN file-transfer
//! example to slice an in-memory file into uplink-sized chunks emitted at a
//! fixed minimum cadence. It is the boundary between the application's payload
//! supply and the LoRaWAN adapter's send loop, so its invariants are the
//! contract the adapter relies on:
//!
//! - **Conservation:** the concatenation of all emitted chunks equals the
//!   original `data` exactly once — no bytes lost, duplicated, or reordered.
//! - **Throttle:** `next_payload(time)` returns `None` whenever
//!   `time < next_send`, where `next_send` is set to `time + interval_us`
//!   after each successful emission.
//! - **Exhaustion = termination:** once every byte has been emitted,
//!   `is_done()` is `true`, `next_payload` permanently returns `None`, and
//!   `next_available_time` returns `None` — never `Some(_)`. This is what lets
//!   the adapter stop scheduling the node.
//! - **`next_available_time` is idempotent (read-only) and consistent with
//!   `next_payload`:** if it returns `Some(t)`, then `next_payload(t)` would
//!   succeed (assuming no intervening mutation); if it returns `None`, so does
//!   `next_payload` for any future time.
//!
//! Preconditions: `chunk_size >= 1`. With `chunk_size == 0`, `next_payload`
//! would emit empty chunks indefinitely without advancing `offset` — that case
//! is the caller's responsibility (the example uses a constant > 0).

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
    use proptest::prelude::*;

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

    /// `is_done` and `next_available_time` must agree at every step: as long
    /// as bytes remain, scheduling info is available; once exhausted, both
    /// terminate. This is the invariant the LoRaWAN adapter relies on to stop
    /// rescheduling the node.
    #[test]
    fn is_done_and_next_available_time_agree() {
        let mut f = FileFragmenter::new(vec![1, 2, 3, 4, 5], 2, 0);
        while !f.is_done() {
            assert!(
                f.next_available_time(0).is_some(),
                "while not done, next_available_time must be Some"
            );
            f.next_payload(0).expect("should produce while not done");
        }
        assert!(f.is_done());
        assert_eq!(
            f.next_available_time(0),
            None,
            "exhausted fragmenter must return None for next_available_time"
        );
        assert_eq!(
            f.next_available_time(u64::MAX),
            None,
            "exhausted state must persist for any future time"
        );
    }

    /// When `current_time < next_send`, `next_available_time` reports the
    /// earliest legal send time. Calling `next_payload` at exactly that time
    /// must succeed.
    #[test]
    fn next_available_time_points_at_legal_send_time() {
        let mut f = FileFragmenter::new(vec![1, 2, 3, 4], 2, 1_000);
        assert!(f.next_payload(0).is_some());
        // Before next_send: must point forward to next_send.
        let earliest = f.next_available_time(500).expect("not yet exhausted");
        assert_eq!(earliest, 1_000);
        // At earliest, next_payload must succeed.
        assert!(f.next_payload(earliest).is_some());
    }

    /// Empty data exhausts immediately: no payload is ever produced and the
    /// fragmenter signals termination from the first observation.
    #[test]
    fn empty_data_is_immediately_done() {
        let mut f = FileFragmenter::new(vec![], 16, 1_000);
        assert!(f.is_done());
        assert_eq!(f.next_available_time(0), None);
        assert_eq!(f.next_payload(0), None);
    }

    /// When `chunk_size > data.len()`, the entire file is emitted as a single
    /// short chunk and the fragmenter terminates.
    #[test]
    fn chunk_larger_than_data_emits_single_short_chunk() {
        let mut f = FileFragmenter::new(vec![1, 2, 3], 16, 0);
        assert_eq!(f.next_payload(0), Some(vec![1, 2, 3]));
        assert!(f.is_done());
        assert_eq!(f.next_payload(0), None);
    }

    /// `next_available_time` is a pure read: calling it many times must not
    /// change the fragmenter's behavior.
    #[test]
    fn next_available_time_is_pure() {
        let mut f = FileFragmenter::new(vec![1, 2, 3, 4], 2, 1_000);
        assert!(f.next_payload(0).is_some());
        for t in 0..2_000 {
            let _ = f.next_available_time(t);
        }
        // Second chunk must still be retrievable at t=1_000 and equal [3,4].
        assert_eq!(f.next_payload(1_000), Some(vec![3, 4]));
    }

    proptest! {
        /// Conservation: the concatenation of every chunk emitted by a
        /// fragmenter equals the original `data`. No bytes lost, duplicated,
        /// or reordered — the file-transfer example's correctness depends on
        /// this exact equality.
        #[test]
        fn chunks_concat_to_original_data(
            data in proptest::collection::vec(any::<u8>(), 0..=200),
            chunk_size in 1usize..=64,
        ) {
            let original = data.clone();
            let mut f = FileFragmenter::new(data, chunk_size, 0);
            let mut reassembled = Vec::new();
            while let Some(chunk) = f.next_payload(0) {
                prop_assert!(
                    chunk.len() <= chunk_size,
                    "chunk {} must not exceed chunk_size {}", chunk.len(), chunk_size,
                );
                prop_assert!(!chunk.is_empty(), "non-final emit must be non-empty");
                reassembled.extend_from_slice(&chunk);
            }
            prop_assert_eq!(reassembled, original);
            prop_assert!(f.is_done());
        }

        /// Throttle: `next_payload` must return `None` for any time strictly
        /// less than `next_send`, and must succeed at `next_send` exactly.
        /// This is what enforces the `interval_us` minimum spacing.
        #[test]
        fn interval_throttles_payload(
            data_len in 2usize..=20,
            chunk_size in 1usize..=4,
            interval in 1u64..=10_000,
        ) {
            let data = vec![0u8; data_len];
            let mut f = FileFragmenter::new(data, chunk_size, interval);
            // First emit succeeds at t=0.
            prop_assert!(f.next_payload(0).is_some());
            // For all t in (0, interval), throttle blocks emission.
            for t in [1u64, interval / 2 + 1, interval - 1] {
                if t == 0 || t >= interval { continue; }
                prop_assert_eq!(f.next_payload(t), None);
            }
            // At t == interval, emit is allowed (assuming data remains).
            if !f.is_done() {
                prop_assert!(f.next_payload(interval).is_some());
            }
        }

        /// Exhaustion is permanent: once `is_done`, no future `next_payload`
        /// or `next_available_time` call can ever produce a value.
        #[test]
        fn exhaustion_is_absorbing(
            data in proptest::collection::vec(any::<u8>(), 1..=50),
            chunk_size in 1usize..=8,
        ) {
            let mut f = FileFragmenter::new(data, chunk_size, 0);
            while f.next_payload(0).is_some() {}
            prop_assert!(f.is_done());
            // Probe a wide range of future times.
            for t in [0u64, 1, 1_000, 1_000_000, u64::MAX / 2, u64::MAX] {
                prop_assert_eq!(f.next_payload(t), None);
                prop_assert_eq!(f.next_available_time(t), None);
            }
        }
    }
}
