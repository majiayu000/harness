pub const DEFAULT_STALL_TIMEOUT_SECS: u64 = 600;
pub const MIN_STALL_TIMEOUT_SECS: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StallTimeoutNormalization {
    pub requested_secs: u64,
    pub effective_secs: u64,
    pub wall_clock_timeout_secs: Option<u64>,
}

impl StallTimeoutNormalization {
    pub fn was_adjusted(self) -> bool {
        self.requested_secs != self.effective_secs
    }
}

pub fn normalize_stall_timeout_secs(
    requested_secs: u64,
    wall_clock_timeout_secs: Option<u64>,
) -> StallTimeoutNormalization {
    let minimum = match wall_clock_timeout_secs {
        Some(timeout_secs) => MIN_STALL_TIMEOUT_SECS
            .min(timeout_secs.saturating_sub(1))
            .max(1),
        None => MIN_STALL_TIMEOUT_SECS,
    };
    let mut effective_secs = requested_secs.max(minimum);

    if let Some(timeout_secs) = wall_clock_timeout_secs {
        if effective_secs >= timeout_secs {
            effective_secs = timeout_secs.saturating_sub(1).max(1);
        }
    }

    StallTimeoutNormalization {
        requested_secs,
        effective_secs,
        wall_clock_timeout_secs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_stall_window_is_ten_minutes() {
        assert_eq!(DEFAULT_STALL_TIMEOUT_SECS, 600);
    }

    #[test]
    fn normalize_floors_short_stall_windows() {
        let normalized = normalize_stall_timeout_secs(5, Some(3600));
        assert_eq!(normalized.effective_secs, 60);
        assert!(normalized.was_adjusted());
    }

    #[test]
    fn normalize_clamps_stall_below_wall_clock_timeout() {
        let normalized = normalize_stall_timeout_secs(3600, Some(3600));
        assert_eq!(normalized.effective_secs, 3599);
        assert!(normalized.was_adjusted());
    }

    #[test]
    fn normalize_keeps_valid_stall_window() {
        let normalized = normalize_stall_timeout_secs(600, Some(3600));
        assert_eq!(normalized.effective_secs, 600);
        assert!(!normalized.was_adjusted());
    }

    #[test]
    fn normalize_handles_tiny_wall_clock_timeout() {
        let normalized = normalize_stall_timeout_secs(60, Some(2));
        assert_eq!(normalized.effective_secs, 1);
        assert!(normalized.was_adjusted());
    }

    #[test]
    fn normalize_handles_one_second_wall_clock_timeout() {
        let normalized = normalize_stall_timeout_secs(600, Some(1));
        assert_eq!(normalized.effective_secs, 1);
        assert!(normalized.was_adjusted());
    }
}
