use chrono::{DateTime, Duration, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

/// Daily maintenance window configuration for scheduled restarts and self-updates.
///
/// When enabled, no new tasks are dispatched during the configured time window.
/// In-flight tasks continue to completion. The window boundary logic handles
/// cross-midnight ranges (e.g. 23:00-02:00) and DST transitions via `chrono-tz`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaintenanceWindowConfig {
    /// When false (default), this feature is completely disabled. No behaviour change.
    #[serde(default)]
    pub enabled: bool,
    /// Start of the quiet window in local time (inclusive). Format: "HH:MM:SS".
    #[serde(default = "default_quiet_window_start")]
    pub quiet_window_start: NaiveTime,
    /// End of the quiet window in local time (exclusive). Format: "HH:MM:SS".
    /// If `quiet_window_end < quiet_window_start`, the window crosses midnight.
    #[serde(default = "default_quiet_window_end")]
    pub quiet_window_end: NaiveTime,
    /// IANA timezone name (e.g. "America/New_York"). Defaults to "UTC".
    #[serde(default = "default_maintenance_timezone")]
    pub timezone: String,
    /// Maximum seconds to wait for in-flight tasks to drain before external restart.
    #[serde(default = "default_drain_timeout_secs")]
    pub drain_timeout_secs: u64,
}

fn default_quiet_window_start() -> NaiveTime {
    // 06:00:00 is always valid; fallback to midnight if chrono invariants change.
    NaiveTime::from_hms_opt(6, 0, 0).unwrap_or(NaiveTime::MIN)
}

fn default_quiet_window_end() -> NaiveTime {
    // 08:00:00 is always valid; fallback to midnight if chrono invariants change.
    NaiveTime::from_hms_opt(8, 0, 0).unwrap_or(NaiveTime::MIN)
}

fn default_maintenance_timezone() -> String {
    "UTC".to_string()
}

fn default_drain_timeout_secs() -> u64 {
    3600
}

impl Default for MaintenanceWindowConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            quiet_window_start: default_quiet_window_start(),
            quiet_window_end: default_quiet_window_end(),
            timezone: default_maintenance_timezone(),
            drain_timeout_secs: default_drain_timeout_secs(),
        }
    }
}

impl MaintenanceWindowConfig {
    /// Returns `true` when `now` falls inside the configured quiet window.
    ///
    /// Always returns `false` when `enabled = false`.
    /// Handles cross-midnight windows: if `quiet_window_end < quiet_window_start`,
    /// the window spans midnight and is active when `time >= start || time < end`.
    /// Equal start and end produce a zero-length window (always returns `false`).
    /// Falls back to UTC if the timezone string cannot be parsed.
    pub fn in_quiet_window(&self, now: DateTime<Utc>) -> bool {
        if !self.enabled {
            return false;
        }
        let tz = self
            .timezone
            .parse::<chrono_tz::Tz>()
            .unwrap_or(chrono_tz::UTC);
        let local = now.with_timezone(&tz);
        let t = local.time();
        let start = self.quiet_window_start;
        let end = self.quiet_window_end;
        if end < start {
            // Cross-midnight: active from start to midnight, and midnight to end.
            t >= start || t < end
        } else {
            t >= start && t < end
        }
    }

    /// Returns the number of seconds until the quiet window ends.
    ///
    /// Uses timezone-aware `DateTime` arithmetic so the result is correct on
    /// DST transition days. Returns 0 when `enabled = false`.
    pub fn secs_until_window_end(&self, now: DateTime<Utc>) -> u64 {
        if !self.enabled {
            return 0;
        }
        let tz = self
            .timezone
            .parse::<chrono_tz::Tz>()
            .unwrap_or(chrono_tz::UTC);
        let local: chrono::DateTime<chrono_tz::Tz> = now.with_timezone(&tz);
        let today = local.date_naive();
        let end_naive = today.and_time(self.quiet_window_end);
        // earliest() resolves DST ambiguity (fall-back); returns None on spring-forward
        // gaps, which we treat as "window end has passed".
        let diff_secs = match tz.from_local_datetime(&end_naive).earliest() {
            Some(end_dt) => end_dt.signed_duration_since(local).num_seconds(),
            None => 0,
        };
        if diff_secs > 0 {
            diff_secs as u64
        } else {
            // End time has already passed today; for cross-midnight windows the end
            // falls tomorrow, so compute against tomorrow's wall-clock end time.
            let tomorrow = today + Duration::days(1);
            let end_tomorrow_naive = tomorrow.and_time(self.quiet_window_end);
            tz.from_local_datetime(&end_tomorrow_naive)
                .earliest()
                .map(|end_dt| end_dt.signed_duration_since(local).num_seconds().max(0) as u64)
                .unwrap_or(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc_at(hour: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2024, 3, 15, hour, min, 0)
            .single()
            .expect("valid datetime")
    }

    fn window(start_h: u32, start_m: u32, end_h: u32, end_m: u32) -> MaintenanceWindowConfig {
        MaintenanceWindowConfig {
            enabled: true,
            quiet_window_start: NaiveTime::from_hms_opt(start_h, start_m, 0).unwrap(),
            quiet_window_end: NaiveTime::from_hms_opt(end_h, end_m, 0).unwrap(),
            timezone: "UTC".to_string(),
            drain_timeout_secs: 3600,
        }
    }

    #[test]
    fn disabled_always_false() {
        let mut cfg = window(6, 0, 8, 0);
        cfg.enabled = false;
        assert!(!cfg.in_quiet_window(utc_at(7, 0)));
        assert!(!cfg.in_quiet_window(utc_at(6, 0)));
    }

    #[test]
    fn inside_window() {
        let cfg = window(6, 0, 8, 0);
        assert!(cfg.in_quiet_window(utc_at(7, 0)));
    }

    #[test]
    fn before_window() {
        let cfg = window(6, 0, 8, 0);
        assert!(!cfg.in_quiet_window(utc_at(5, 59)));
    }

    #[test]
    fn after_window() {
        let cfg = window(6, 0, 8, 0);
        assert!(!cfg.in_quiet_window(utc_at(8, 1)));
    }

    #[test]
    fn boundary_start_inclusive() {
        let cfg = window(6, 0, 8, 0);
        assert!(cfg.in_quiet_window(utc_at(6, 0)));
    }

    #[test]
    fn boundary_end_exclusive() {
        let cfg = window(6, 0, 8, 0);
        assert!(!cfg.in_quiet_window(utc_at(8, 0)));
    }

    #[test]
    fn cross_midnight_inside() {
        let cfg = window(23, 0, 2, 0);
        assert!(cfg.in_quiet_window(utc_at(0, 30)));
    }

    #[test]
    fn cross_midnight_outside() {
        let cfg = window(23, 0, 2, 0);
        assert!(!cfg.in_quiet_window(utc_at(12, 0)));
    }

    #[test]
    fn dst_spring_forward_no_panic() {
        let cfg = MaintenanceWindowConfig {
            enabled: true,
            quiet_window_start: NaiveTime::from_hms_opt(2, 0, 0).unwrap(),
            quiet_window_end: NaiveTime::from_hms_opt(4, 0, 0).unwrap(),
            timezone: "America/New_York".to_string(),
            drain_timeout_secs: 3600,
        };
        let now = Utc
            .with_ymd_and_hms(2024, 3, 10, 7, 30, 0)
            .single()
            .unwrap();
        cfg.in_quiet_window(now);
    }

    #[test]
    fn dst_fall_back_no_panic() {
        let cfg = MaintenanceWindowConfig {
            enabled: true,
            quiet_window_start: NaiveTime::from_hms_opt(1, 0, 0).unwrap_or(NaiveTime::MIN),
            quiet_window_end: NaiveTime::from_hms_opt(3, 0, 0).unwrap_or(NaiveTime::MIN),
            timezone: "America/New_York".to_string(),
            drain_timeout_secs: 3600,
        };
        let now = Utc
            .with_ymd_and_hms(2024, 11, 3, 6, 30, 0)
            .single()
            .unwrap();
        cfg.in_quiet_window(now);
    }

    #[test]
    fn secs_until_end_normal() {
        let cfg = window(6, 0, 8, 0);
        let secs = cfg.secs_until_window_end(utc_at(7, 0));
        assert_eq!(secs, 3600);
    }

    #[test]
    fn secs_until_end_cross_midnight() {
        let cfg = window(23, 0, 2, 0);
        let secs = cfg.secs_until_window_end(utc_at(23, 30));
        assert_eq!(secs, 9000);
    }

    #[test]
    fn maintenance_window_config_default_disabled() {
        let cfg = MaintenanceWindowConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.timezone, "UTC");
        assert_eq!(cfg.drain_timeout_secs, 3600);
    }

    #[test]
    fn maintenance_window_config_toml_roundtrip() {
        let toml = r#"
            enabled = true
            quiet_window_start = "06:00:00"
            quiet_window_end = "08:00:00"
            timezone = "America/New_York"
            drain_timeout_secs = 7200
        "#;
        let cfg: MaintenanceWindowConfig = toml::from_str(toml).expect("toml parse failed");
        assert!(cfg.enabled);
        assert_eq!(cfg.timezone, "America/New_York");
        assert_eq!(cfg.drain_timeout_secs, 7200);
    }
}
