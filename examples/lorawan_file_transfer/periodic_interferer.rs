use theatron::time::SimTime;
use theatron::traits::InterferenceSource;
use theatron::types::{ChannelEvent, Transmission};

pub struct PeriodicInterferer {
    period_us: u64,
    sf: u8,
    frequency: u32,
    duration_us: u64,
    tx_power_dbm: i8,
}

impl PeriodicInterferer {
    pub fn new(period_us: u64, sf: u8, frequency: u32, duration_us: u64) -> Self {
        Self {
            period_us,
            sf,
            frequency,
            duration_us,
            tx_power_dbm: 14,
        }
    }
}

impl InterferenceSource for PeriodicInterferer {
    fn observe(&mut self, _event: &ChannelEvent, _time: SimTime) {}

    fn poll_inject(&mut self, _time: SimTime) -> Option<Transmission> {
        Some(Transmission {
            payload: vec![0xFF],
            sf: self.sf,
            bandwidth: 125_000,
            coding_rate: 5,
            frequency: self.frequency,
            duration_us: self.duration_us,
            tx_power_dbm: self.tx_power_dbm,
        })
    }

    fn next_poll_time(&self, current_time: SimTime) -> Option<SimTime> {
        Some(current_time + self.period_us)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_interferer() -> PeriodicInterferer {
        PeriodicInterferer::new(10_000_000, 7, 868_100_000, 500_000)
    }

    #[test]
    fn always_injects() {
        let mut interferer = make_interferer();
        let tx = interferer
            .poll_inject(0)
            .expect("should always return Some");
        assert_eq!(tx.sf, 7);
        assert_eq!(tx.frequency, 868_100_000);
        assert_eq!(tx.payload, vec![0xFF]);
        assert_eq!(tx.duration_us, 500_000);
    }

    #[test]
    fn next_poll_time_advances_by_period() {
        let interferer = make_interferer();
        assert_eq!(interferer.next_poll_time(0), Some(10_000_000));
        assert_eq!(interferer.next_poll_time(10_000_000), Some(20_000_000));
        assert_eq!(interferer.next_poll_time(55_000_000), Some(65_000_000));
    }

    #[test]
    fn observe_is_noop() {
        let mut interferer = make_interferer();
        let event = ChannelEvent::TransmissionStarted {
            sender: theatron::types::NodeId(1),
            sf: 7,
            frequency: 868_100_000,
            time: 0,
        };
        interferer.observe(&event, 0);
        let tx = interferer
            .poll_inject(0)
            .expect("still injects after observe");
        assert_eq!(tx.sf, 7);
    }
}
