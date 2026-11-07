//! Rate limits: how much one player changed recently, by the change times in the log.

use std::collections::HashMap;

use crate::settings::RateLimits;

const MINUTE: u64 = 60;
const TEN_MINUTES: u64 = 600;

#[derive(Clone, Debug, Default)]
pub struct Rates {
    /// Recent changes per player: when, and how many voxels.
    recent: HashMap<String, Vec<(u64, usize)>>,
}

impl Rates {
    /// Whether `player` may make a change costing `cost` voxels at time `at`.
    pub fn check(&self, limits: &RateLimits, player: &str, at: u64, cost: usize) -> Result<(), String> {
        let recent = self.recent.get(player).map(Vec::as_slice).unwrap_or_default();
        if let Some(n) = limits.changes_per_minute {
            let count = recent.iter().filter(|(t, _)| t + MINUTE > at).count();
            if count + 1 > n as usize {
                return Err(format!("you're changing things too fast: this server allows {n} changes a minute"));
            }
        }
        if let Some(max) = limits.voxels_per_ten_minutes {
            let sum: usize = recent.iter().filter(|(t, _)| t + TEN_MINUTES > at).map(|(_, c)| c).sum();
            if sum + cost > max {
                return Err(format!(
                    "this would make {} voxels changed in the last 10 minutes; this server allows {max}",
                    sum + cost
                ));
            }
        }
        Ok(())
    }

    pub fn add(&mut self, player: &str, at: u64, cost: usize) {
        let recent = self.recent.entry(player.to_string()).or_default();
        recent.retain(|(t, _)| t + TEN_MINUTES > at);
        recent.push((at, cost));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_per_minute_and_per_ten_minutes() {
        let limits = RateLimits { changes_per_minute: Some(2), voxels_per_ten_minutes: Some(1000) };
        let mut rates = Rates::default();
        rates.add("a", 100, 400);
        rates.add("a", 110, 400);
        assert!(rates.check(&limits, "a", 120, 0).unwrap_err().contains("2 changes a minute"));
        assert!(rates.check(&limits, "b", 120, 0).is_ok());
        // A minute later the count resets, but the voxels still add up.
        assert!(rates.check(&limits, "a", 175, 100).is_ok());
        assert!(rates.check(&limits, "a", 175, 300).unwrap_err().contains("1100"));
        assert!(rates.check(&limits, "a", 800, 900).is_ok());
        assert!(rates.check(&RateLimits::default(), "a", 120, 1_000_000).is_ok());
    }
}
