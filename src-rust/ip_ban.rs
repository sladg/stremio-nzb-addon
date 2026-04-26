//! In-process IP ban list for fail2ban-style auto-blocking.
//!
//! Tracks bad-token rejections per client IP. When an IP exceeds
//! `failure_threshold` rejections within `failure_window`, it gets
//! banned for `ban_duration`. Banned IPs are short-circuited at the
//! outer middleware before any other work runs.
//!
//! All state is in-memory — restarts wipe the ban list. For a single-
//! container deployment this is fine; for multi-instance setups a
//! reverse proxy with shared state (Caddy + fail2ban, CF WAF) is the
//! right answer.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;

#[derive(Debug, Clone)]
pub struct BanConfig {
    /// Bad-token rejections within `window` that flip an IP from
    /// "tolerated" to "banned". 0 disables the ban tracker entirely.
    pub failure_threshold: u32,
    /// Sliding window in which `failure_threshold` failures count.
    /// Failures older than this are forgotten on the next access.
    pub window: Duration,
    /// How long an IP stays banned after crossing the threshold.
    pub ban_duration: Duration,
}

impl BanConfig {
    pub fn from_env() -> Self {
        let failure_threshold = std::env::var("BAN_FAILURE_THRESHOLD")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5);
        let window_secs = std::env::var("BAN_WINDOW_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300);
        let ban_secs = std::env::var("BAN_DURATION_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3600);
        Self {
            failure_threshold,
            window: Duration::from_secs(window_secs),
            ban_duration: Duration::from_secs(ban_secs),
        }
    }

    pub fn enabled(&self) -> bool {
        self.failure_threshold > 0
    }
}

/// Per-IP tracking state. Failure timestamps are kept as a small
/// VecDeque-like Vec — we prune stale entries lazily on each access,
/// so the inner Vec only holds entries within `window`.
#[derive(Debug, Default)]
struct IpState {
    failures: Vec<Instant>,
    banned_until: Option<Instant>,
}

#[derive(Debug)]
pub struct BanList {
    config: BanConfig,
    state: DashMap<IpAddr, IpState>,
}

impl BanList {
    pub fn new(config: BanConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            state: DashMap::new(),
        })
    }

    /// True if `ip` is currently banned. Lazily clears expired bans on
    /// access (so a long-lived banned-IP entry self-heals after the
    /// duration expires, no GC pass needed).
    pub fn is_banned(&self, ip: IpAddr) -> bool {
        if !self.config.enabled() {
            return false;
        }
        let mut entry = match self.state.get_mut(&ip) {
            Some(e) => e,
            None => return false,
        };
        let now = Instant::now();
        if let Some(until) = entry.banned_until {
            if now < until {
                return true;
            }
            // Ban expired — clear it. Failure history starts fresh.
            entry.banned_until = None;
            entry.failures.clear();
        }
        false
    }

    /// Record one failed-auth event from `ip`. If the count of failures
    /// inside `window` reaches `failure_threshold`, the IP is banned.
    /// Returns `true` if this call triggered the ban (caller can log
    /// the transition); `false` otherwise.
    pub fn record_failure(&self, ip: IpAddr) -> bool {
        if !self.config.enabled() {
            return false;
        }
        let now = Instant::now();
        let cutoff = now.checked_sub(self.config.window).unwrap_or(now);

        let mut entry = self.state.entry(ip).or_default();
        // Already banned — bump nothing, just stay quiet.
        if let Some(until) = entry.banned_until {
            if now < until {
                return false;
            }
        }
        // Drop stale failures from outside the window.
        entry.failures.retain(|t| *t >= cutoff);
        entry.failures.push(now);

        if entry.failures.len() as u32 >= self.config.failure_threshold {
            entry.banned_until = Some(now + self.config.ban_duration);
            entry.failures.clear();
            return true;
        }
        false
    }

    /// Snapshot of currently-banned IPs (for logs / inspection).
    /// O(N); intended for ad-hoc use, not hot paths.
    pub fn currently_banned(&self) -> Vec<IpAddr> {
        let now = Instant::now();
        self.state
            .iter()
            .filter(|e| e.value().banned_until.is_some_and(|until| now < until))
            .map(|e| *e.key())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn small_config() -> BanConfig {
        BanConfig {
            failure_threshold: 3,
            window: Duration::from_secs(10),
            ban_duration: Duration::from_secs(60),
        }
    }

    #[test]
    fn disabled_when_threshold_zero() {
        let bl = BanList::new(BanConfig {
            failure_threshold: 0,
            window: Duration::from_secs(10),
            ban_duration: Duration::from_secs(60),
        });
        assert!(!bl.config.enabled());
        assert!(!bl.record_failure(ip("1.2.3.4")));
        assert!(!bl.is_banned(ip("1.2.3.4")));
    }

    #[test]
    fn under_threshold_not_banned() {
        let bl = BanList::new(small_config());
        let addr = ip("10.0.0.1");
        assert!(!bl.record_failure(addr));
        assert!(!bl.record_failure(addr));
        assert!(!bl.is_banned(addr));
    }

    #[test]
    fn at_threshold_triggers_ban() {
        let bl = BanList::new(small_config());
        let addr = ip("10.0.0.2");
        assert!(!bl.record_failure(addr));
        assert!(!bl.record_failure(addr));
        // 3rd failure trips the ban.
        assert!(bl.record_failure(addr));
        assert!(bl.is_banned(addr));
    }

    #[test]
    fn distinct_ips_tracked_independently() {
        let bl = BanList::new(small_config());
        let a = ip("10.0.0.3");
        let b = ip("10.0.0.4");
        bl.record_failure(a);
        bl.record_failure(a);
        bl.record_failure(b);
        // a is one rec away from ban; b is at one. Neither banned yet.
        assert!(!bl.is_banned(a));
        assert!(!bl.is_banned(b));
        bl.record_failure(a);
        assert!(bl.is_banned(a));
        assert!(!bl.is_banned(b));
    }

    #[test]
    fn currently_banned_lists_active_only() {
        let bl = BanList::new(small_config());
        let a = ip("10.0.0.5");
        let b = ip("10.0.0.6");
        for _ in 0..3 {
            bl.record_failure(a);
        }
        bl.record_failure(b);
        let banned = bl.currently_banned();
        assert_eq!(banned, vec![a]);
    }
}
