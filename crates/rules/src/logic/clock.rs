//! World time: days and seasons, from the change times in the log. Every program
//! reading the same log agrees on the date, so rules can depend on it.

use serde::{Deserialize, Serialize};

pub const SEASONS: [&str; 4] = ["spring", "summer", "autumn", "winter"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClockRules {
    /// Unix seconds when day 0 began.
    pub epoch: u64,
    /// Real seconds in one game day.
    pub day_seconds: u32,
    pub days_per_season: u32,
}

impl Default for ClockRules {
    fn default() -> Self {
        Self {
            epoch: 0,
            day_seconds: 1200,
            days_per_season: 28,
        }
    }
}

/// The date and time of day at some moment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WorldTime {
    /// Days since the epoch.
    pub day: u64,
    pub year: u64,
    /// Index into [`SEASONS`].
    pub season: usize,
    pub day_of_season: u32,
    /// 0 at the start of the day, up to (not reaching) 1.
    pub time_of_day: f32,
}

impl ClockRules {
    pub fn at(&self, unix: u64) -> WorldTime {
        let day_seconds = self.day_seconds.max(1) as u64;
        let per_season = self.days_per_season.max(1) as u64;
        let since = unix.saturating_sub(self.epoch);
        let day = since / day_seconds;
        let season = day / per_season;
        WorldTime {
            day,
            year: season / SEASONS.len() as u64,
            season: (season % SEASONS.len() as u64) as usize,
            day_of_season: (day % per_season) as u32,
            time_of_day: (since % day_seconds) as f32 / day_seconds as f32,
        }
    }

    /// Days that began after `from` and by `to`: the `day_start` events between two
    /// changes.
    pub fn days_started(&self, from: u64, to: u64) -> std::ops::Range<u64> {
        let (a, b) = (self.at(from).day, self.at(to).day);
        a + 1..b.max(a) + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        let clock = ClockRules {
            epoch: 1000,
            day_seconds: 100,
            days_per_season: 2,
        };
        let t = clock.at(1000 + 100 * 9 + 25);
        assert_eq!((t.day, t.year, t.season, t.day_of_season), (9, 1, 0, 1));
        assert_eq!(t.time_of_day, 0.25);
        assert_eq!(clock.at(0).day, 0);
        assert_eq!(clock.days_started(1050, 1350), 1..4);
        assert!(clock.days_started(1350, 1050).is_empty());
    }
}
