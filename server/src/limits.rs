//! The per-IP flood guard and each site's fetch budget (extractions per minute and at once).

use std::net::IpAddr;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use governor::{DefaultDirectRateLimiter, DefaultKeyedRateLimiter, Quota, RateLimiter};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::extract::Source;

// The limits run on the kernel's monotonic clock, which a suspend leaves alone. quanta's (governor's
// default) reads the CPU's TSC, which a suspend can reset: time then goes back by as long as the
// machine had been up, and every budget and limit refuses all requests until it catches up. This
// stops compiling if a dependency turns quanta back on.
const _: fn(governor::clock::DefaultClock) -> governor::clock::MonotonicClock = |clock| clock;

fn per_minute(n: u32) -> Quota {
    Quota::per_minute(NonZeroU32::new(n.max(1)).expect("at least 1"))
}

/// Requests per IP per minute on the public listener.
pub struct IpLimiter {
    state: ArcSwap<(u32, DefaultKeyedRateLimiter<IpAddr>)>,
}

impl IpLimiter {
    pub fn new(limit: u32) -> Self {
        Self { state: ArcSwap::from_pointee((limit, RateLimiter::keyed(per_minute(limit)))) }
    }

    pub fn allow(&self, ip: IpAddr, limit: u32) -> bool {
        let state = self.state.load();
        if state.0 == limit {
            return state.1.check_key(&ip).is_ok();
        }
        let fresh = Arc::new((limit, RateLimiter::keyed(per_minute(limit))));
        let allowed = fresh.1.check_key(&ip).is_ok();
        self.state.store(fresh);
        allowed
    }

    /// Forgets IPs that are back to a full allowance.
    pub fn cleanup(&self) {
        let state = self.state.load();
        state.1.retain_recent();
        state.1.shrink_to_fit();
    }
}

struct BudgetState {
    per_minute: u32,
    concurrency: u32,
    rate: DefaultDirectRateLimiter,
    slots: Arc<Semaphore>,
}

/// Every site's budget, so one that blocks can't use up another's.
pub struct Budgets {
    sites: [Budget; Source::ALL.len()],
    /// The share of each site's fetches Discord may use: anyone can claim to be Discord, and
    /// mustn't be able to spend Telegram's.
    discord: [Budget; Source::ALL.len()],
}

impl Budgets {
    pub fn new(settings: &crate::settings::Settings) -> Self {
        Self {
            sites: Source::ALL.map(|source| {
                let s = settings.source(source);
                Budget::new(s.fetches_per_minute, s.concurrency)
            }),
            discord: Source::ALL.map(|source| {
                let s = settings.source(source);
                Budget::new(discord_share(s.fetches_per_minute), discord_share(s.concurrency))
            }),
        }
    }

    pub const fn of(&self, source: Source) -> &Budget {
        &self.sites[source.index()]
    }

    /// Discord's share of `source`'s fetches, taken before the site's own budget.
    pub const fn discord(&self, source: Source) -> &Budget {
        &self.discord[source.index()]
    }
}

/// Discord's share of a site's fetches a minute, or of those at once: half, rounded down. None
/// (from a site allowed one) means Discord gets only posts already cached.
pub const fn discord_share(limit: u32) -> u32 {
    limit / 2
}

/// Extractions from one site: a rate per minute and a cap on concurrent ones.
pub struct Budget {
    state: ArcSwap<BudgetState>,
    exhausted: AtomicU64,
}

impl Budget {
    pub fn new(per_minute: u32, concurrency: u32) -> Self {
        Self { state: ArcSwap::from_pointee(Self::build(per_minute, concurrency)), exhausted: AtomicU64::new(0) }
    }

    fn build(per_minute_limit: u32, concurrency: u32) -> BudgetState {
        BudgetState {
            per_minute: per_minute_limit,
            concurrency,
            rate: RateLimiter::direct(per_minute(per_minute_limit)),
            slots: Arc::new(Semaphore::new(concurrency.max(1) as usize)),
        }
    }

    fn current(&self, per_minute: u32, concurrency: u32) -> Arc<BudgetState> {
        let state = self.state.load_full();
        if state.per_minute == per_minute && state.concurrency == concurrency {
            return state;
        }
        let fresh = Arc::new(Self::build(per_minute, concurrency));
        self.state.store(Arc::clone(&fresh));
        fresh
    }

    /// Waits up to `wait` for a fetch slot within the rate. Holding the permit holds the slot.
    /// The slot comes first: a request that never gets one mustn't use up the minute's rate.
    pub async fn acquire(&self, per_minute: u32, concurrency: u32, wait: Duration) -> Option<OwnedSemaphorePermit> {
        let state = self.current(per_minute, concurrency);
        let acquired = tokio::time::timeout(wait, async {
            let permit = Arc::clone(&state.slots).acquire_owned().await;
            state.rate.until_ready().await;
            permit
        })
        .await;
        if let Ok(Ok(permit)) = acquired {
            Some(permit)
        } else {
            self.exhausted.fetch_add(1, Ordering::Relaxed);
            None
        }
    }

    /// Fetches running now.
    pub fn in_use(&self) -> u32 {
        let state = self.state.load();
        state.concurrency.saturating_sub(u32::try_from(state.slots.available_permits()).unwrap_or(u32::MAX))
    }

    /// How many times a fetch found no budget, since startup.
    pub fn exhausted(&self) -> u64 {
        self.exhausted.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ip_limit() {
        let limiter = IpLimiter::new(3);
        let a: IpAddr = "1.1.1.1".parse().unwrap();
        let b: IpAddr = "2.2.2.2".parse().unwrap();
        assert!((0..3).all(|_| limiter.allow(a, 3)));
        assert!(!limiter.allow(a, 3));
        assert!(limiter.allow(b, 3));
        // A new limit applies right away.
        assert!(limiter.allow(a, 10));
    }

    #[tokio::test]
    async fn budget_caps_concurrency_and_rate() {
        let budget = Budget::new(100, 2);
        let wait = Duration::from_millis(50);
        let first = budget.acquire(100, 2, wait).await.unwrap();
        let _second = budget.acquire(100, 2, wait).await.unwrap();
        assert_eq!(budget.in_use(), 2);
        assert!(budget.acquire(100, 2, wait).await.is_none(), "no free slot");
        drop(first);
        assert!(budget.acquire(100, 2, wait).await.is_some());

        let budget = Budget::new(1, 5);
        drop(budget.acquire(1, 5, wait).await.unwrap());
        assert!(budget.acquire(1, 5, wait).await.is_none(), "rate used up");
        assert_eq!(budget.exhausted(), 1);
    }
}
