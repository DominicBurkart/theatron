/// Simulation time in microseconds.
///
/// # Examples
///
/// ```
/// use theatron::time::SimTime;
/// let t: SimTime = 1_000;
/// assert_eq!(t, 1_000);
/// ```
pub type SimTime = u64;

/// Convert simulation time (microseconds) to milliseconds.
///
/// # Examples
///
/// ```
/// use theatron::time::sim_time_to_ms;
/// assert_eq!(sim_time_to_ms(5_000), 5);
/// ```
pub fn sim_time_to_ms(t: SimTime) -> u32 {
    (t / 1_000) as u32
}

/// Convert milliseconds to simulation time (microseconds).
///
/// # Examples
///
/// ```
/// use theatron::time::ms_to_sim_time;
/// assert_eq!(ms_to_sim_time(5), 5_000);
/// ```
pub fn ms_to_sim_time(ms: u32) -> SimTime {
    ms as u64 * 1_000
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn zero_round_trips() {
        assert_eq!(ms_to_sim_time(0), 0);
        assert_eq!(sim_time_to_ms(0), 0);
    }

    #[test]
    fn one_ms_is_1000_us() {
        assert_eq!(ms_to_sim_time(1), 1_000);
        assert_eq!(sim_time_to_ms(1_000), 1);
    }

    proptest! {
        #[test]
        fn ms_to_sim_and_back_is_approx(ms in 0u32..1_000_000u32) {
            let sim = ms_to_sim_time(ms);
            let back = sim_time_to_ms(sim);
            prop_assert_eq!(back, ms);
        }

        #[test]
        fn sim_time_monotone(a in 0u64..1_000_000_000u64, b in 0u64..1_000_000_000u64) {
            if a <= b {
                prop_assert!(sim_time_to_ms(a) <= sim_time_to_ms(b));
            }
        }
    }
}
