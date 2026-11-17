use std::time::{Duration, Instant};

use crate::connection::RetryableFailure;

pub(super) struct RetryPolicy {
    enabled: bool,
    attempt: usize,
    pending: Option<Instant>,
}

impl RetryPolicy {
    pub(super) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            attempt: 0,
            pending: None,
        }
    }

    pub(super) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(super) fn restart(&mut self, enabled: bool) {
        *self = Self::new(enabled);
    }

    pub(super) fn waiting(&self) -> bool {
        self.pending.is_some()
    }

    pub(super) fn schedule(&mut self, now: Instant, failure: RetryableFailure) -> Option<Duration> {
        if !self.enabled {
            return None;
        }
        const DELAYS: [u64; 5] = [5, 15, 30, 60, 300];
        if failure.stable_run {
            self.attempt = 0;
        }
        let seconds = DELAYS[self.attempt];
        self.attempt = (self.attempt + 1).min(DELAYS.len() - 1);
        let random = getrandom::u32().unwrap_or(u32::MAX / 2);
        let jitter = 0.9 + 0.2 * (f64::from(random) / f64::from(u32::MAX));
        let delay = Duration::from_secs_f64(seconds as f64 * jitter);
        self.pending = Some(now + delay);
        Some(delay)
    }

    pub(super) fn take_due(&mut self, now: Instant) -> bool {
        if self.pending.is_some_and(|deadline| now >= deadline) {
            self.pending = None;
            true
        } else {
            false
        }
    }

    pub(super) fn cancel(&mut self) -> bool {
        self.enabled = false;
        self.pending.take().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_only_backoff_jitters_caps_resets_and_cancels() {
        let now = Instant::now();
        let transient = RetryableFailure { stable_run: false };
        assert!(RetryPolicy::new(false).schedule(now, transient).is_none());

        let mut policy = RetryPolicy::new(true);
        for seconds in [5, 15, 30, 60, 300, 300] {
            let delay = policy.schedule(now, transient).unwrap().as_secs_f64();
            let seconds = f64::from(seconds);
            assert!((seconds * 0.8999..=seconds * 1.1001).contains(&delay));
            assert!(policy.waiting());
            assert!(!policy.take_due(now));
            assert!(policy.take_due(now + Duration::from_secs(331)));
            assert!(!policy.waiting());
        }
        let recovered = policy
            .schedule(now, RetryableFailure { stable_run: true })
            .unwrap();
        assert!((4.499..=5.501).contains(&recovered.as_secs_f64()));
        assert!(policy.cancel());
        assert!(!policy.take_due(now + recovered));
        assert!(policy.schedule(now, transient).is_none());
        policy.restart(true);
        let resumed = policy.schedule(now, transient).unwrap();
        assert!((4.499..=5.501).contains(&resumed.as_secs_f64()));
        policy.restart(false);
        assert!(!policy.waiting());
        assert!(policy.schedule(now, transient).is_none());
    }
}
