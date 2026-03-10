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
}
