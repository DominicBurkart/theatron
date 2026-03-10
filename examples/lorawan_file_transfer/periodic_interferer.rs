use theatron::time::SimTime;
use theatron::traits::InterferenceSource;
use theatron::types::{ChannelEvent, Transmission};

pub struct PeriodicInterferer {
    period_us: u64,
    sf: u8,
    frequency: u32,
    duration_us: u64,
}

impl PeriodicInterferer {
    pub fn new(period_us: u64, sf: u8, frequency: u32, duration_us: u64) -> Self {
        Self {
            period_us,
            sf,
            frequency,
            duration_us,
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
        })
    }

    fn next_poll_time(&self, current_time: SimTime) -> Option<SimTime> {
        Some(current_time + self.period_us)
    }
}
