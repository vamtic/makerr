//! Adaptive concurrency control (AIMD).
//!
//! The fixed limits the V1 app used are wrong in both directions: too low on a
//! healthy server, too high while the server is throttling. Every fetch reports
//! its outcome here and the limit grows additively on success, then halves on
//! throttling (HTTP 429/503) — the same idea TCP congestion control uses.
//!
//! The limiter is also a plain permit pool: [`AdaptiveLimiter::fixed`] gives the
//! old, predictable behaviour when the user disables `adaptive_concurrency`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

/// How one request finished, from the limiter's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The server answered (including a legitimate 404).
    Ok,
    /// The server explicitly pushed back (429/503 or `Retry-After`).
    Throttled,
    /// A transient failure (timeout, reset). Not server pressure, but it must
    /// not count towards growth either.
    Failed,
}

struct State {
    limit: usize,
    in_flight: usize,
    successes: usize,
    /// No growth until this instant, so a single throttled burst does not get
    /// immediately undone by the requests that were already in flight.
    cooldown_until: Option<Instant>,
}

/// A permit pool whose size adapts to the server's responses.
pub struct AdaptiveLimiter {
    state: Mutex<State>,
    notify: Notify,
    min: usize,
    max: usize,
    cooldown: Duration,
}

impl AdaptiveLimiter {
    /// A limiter that starts at `initial` and may grow up to `max`, never
    /// dropping below `min`.
    pub fn new(initial: usize, min: usize, max: usize) -> Self {
        Self::with_cooldown(initial, min, max, Duration::from_millis(750))
    }

    /// A limiter with a fixed permit count: no reporting, no growth.
    pub fn fixed(permits: usize) -> Self {
        let n = permits.max(1);
        Self {
            state: Mutex::new(State {
                limit: n,
                in_flight: 0,
                successes: 0,
                cooldown_until: None,
            }),
            notify: Notify::new(),
            min: n,
            max: n,
            cooldown: Duration::ZERO,
        }
    }

    pub(crate) fn with_cooldown(
        initial: usize,
        min: usize,
        max: usize,
        cooldown: Duration,
    ) -> Self {
        let max = max.max(1);
        let min = min.clamp(1, max);
        let initial = initial.clamp(min, max);
        Self {
            state: Mutex::new(State {
                limit: initial,
                in_flight: 0,
                successes: 0,
                cooldown_until: None,
            }),
            notify: Notify::new(),
            min,
            max,
            cooldown,
        }
    }

    /// The limiter for tile downloads. `max_concurrent_tiles` is the starting
    /// point and the user's upper hint; the limiter may double it while the
    /// server is happy and shrink to a quarter of it under pressure.
    pub fn for_tiles(adaptive: bool, max_concurrent_tiles: i32) -> Self {
        let base = max_concurrent_tiles.max(1) as usize;
        if !adaptive {
            return Self::fixed(base);
        }
        let max = base.saturating_mul(2).min(256);
        let min = (base / 4).max(2).min(base);
        Self::new(base, min, max)
    }

    /// The limiter for `/history` requests. These are rate limited far more
    /// aggressively than tiles, so growth is capped much lower.
    pub fn for_history(adaptive: bool, max_connections: i32) -> Self {
        let ceiling = max_connections.clamp(1, 500) as usize;
        let seed = ceiling.min(crate::history::HISTORY_SEED_CONCURRENCY);
        if !adaptive {
            return Self::fixed(seed);
        }
        let max = ceiling.min(16).max(seed);
        Self::new(seed, seed.min(2), max)
    }

    /// Current permit count. Exposed for tests and diagnostics.
    pub fn limit(&self) -> usize {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).limit
    }

    /// Waits for a permit. The permit is returned to the pool on drop.
    pub async fn acquire(self: &Arc<Self>) -> Permit {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            // Register the waiter *before* checking the counter, otherwise a
            // release between the check and the wait would be lost.
            notified.as_mut().enable();
            {
                let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                if state.in_flight < state.limit {
                    state.in_flight += 1;
                    return Permit {
                        limiter: Arc::clone(self),
                    };
                }
            }
            notified.await;
        }
    }

    /// Feeds one request's outcome into the AIMD controller.
    pub fn report(&self, outcome: Outcome) {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let mut grew = false;

        match outcome {
            Outcome::Ok => {
                state.successes += 1;
                let cooldown_over = state.cooldown_until.is_none_or(|until| now >= until);
                if cooldown_over && state.successes >= state.limit.max(4) && state.limit < self.max
                {
                    // Multiplicative (+25%, at least one slot): the first
                    // frames of a run are usually the uncached ones, so the
                    // limit has to reach the server's comfort zone within a
                    // handful of frames rather than one slot at a time.
                    let step = (state.limit / 4).max(1);
                    state.limit = (state.limit + step).min(self.max);
                    state.successes = 0;
                    grew = true;
                }
            }
            Outcome::Throttled => {
                state.successes = 0;
                let next = (state.limit / 2).max(self.min);
                if next < state.limit {
                    state.limit = next;
                    state.cooldown_until = Some(now + self.cooldown);
                    grew = true;
                }
            }
            Outcome::Failed => {
                state.successes = 0;
            }
        }

        if grew {
            drop(state);
            self.notify.notify_waiters();
        }
    }

    fn release(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.in_flight = state.in_flight.saturating_sub(1);
        drop(state);
        self.notify.notify_waiters();
    }
}

/// An acquired permit. Releasing it (dropping) wakes one potential waiter.
pub struct Permit {
    limiter: Arc<AdaptiveLimiter>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.limiter.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_multiplicatively_on_success() {
        let lim = AdaptiveLimiter::new(8, 2, 64);
        for _ in 0..8 {
            lim.report(Outcome::Ok);
        }
        assert_eq!(lim.limit(), 10); // 8 + max(8 / 4, 1)
        for _ in 0..10 {
            lim.report(Outcome::Ok);
        }
        assert_eq!(lim.limit(), 12); // 10 + 10 / 4 (integer) = 12
    }

    #[test]
    fn small_limits_still_grow_by_one() {
        let lim = AdaptiveLimiter::new(4, 2, 64);
        for _ in 0..4 {
            lim.report(Outcome::Ok);
        }
        assert_eq!(lim.limit(), 5);
    }

    #[test]
    fn halving_on_throttle_respects_the_floor() {
        let lim = AdaptiveLimiter::with_cooldown(8, 3, 12, Duration::ZERO);
        lim.report(Outcome::Throttled);
        assert_eq!(lim.limit(), 4);
        lim.report(Outcome::Throttled);
        assert_eq!(lim.limit(), 3);
        lim.report(Outcome::Throttled);
        assert_eq!(lim.limit(), 3);
    }

    #[test]
    fn success_does_not_grow_during_cooldown() {
        let lim = AdaptiveLimiter::with_cooldown(8, 2, 12, Duration::from_secs(60));
        lim.report(Outcome::Throttled);
        assert_eq!(lim.limit(), 4);
        for _ in 0..10 {
            lim.report(Outcome::Ok);
        }
        assert_eq!(lim.limit(), 4);
    }

    #[test]
    fn growth_stops_at_the_ceiling() {
        let lim = AdaptiveLimiter::new(11, 2, 12);
        for _ in 0..500 {
            lim.report(Outcome::Ok);
        }
        assert_eq!(lim.limit(), 12);
    }

    #[test]
    fn failures_do_not_shrink_the_limit() {
        let lim = AdaptiveLimiter::new(8, 2, 12);
        for _ in 0..10 {
            lim.report(Outcome::Failed);
        }
        assert_eq!(lim.limit(), 8);
    }

    #[tokio::test]
    async fn fixed_limiter_bounds_in_flight_permits() {
        let lim = Arc::new(AdaptiveLimiter::fixed(2));
        let p1 = lim.acquire().await;
        let p2 = lim.acquire().await;
        assert_eq!(lim.state.lock().unwrap().in_flight, 2);
        let third = tokio::spawn({
            let lim = Arc::clone(&lim);
            async move {
                let p3 = lim.acquire().await;
                drop(p3);
            }
        });
        tokio::task::yield_now().await;
        assert!(!third.is_finished());
        drop(p1);
        third.await.unwrap();
        drop(p2);
        assert_eq!(lim.state.lock().unwrap().in_flight, 0);
    }
}
