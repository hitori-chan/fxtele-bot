//! Reply rate limits per user and per chat, kept in memory.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Token buckets keyed by ID: up to `per_minute` at once, refilling evenly over a minute.
#[derive(Debug)]
struct Limiter {
    per_minute: f64,
    buckets: HashMap<i64, Bucket>,
    last_prune: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Limiter {
    fn new(per_minute: u32) -> Self {
        Self { per_minute: f64::from(per_minute), buckets: HashMap::new(), last_prune: None }
    }

    fn tokens(&self, key: i64, now: Instant) -> f64 {
        self.buckets.get(&key).map_or(self.per_minute, |b| self.refill(*b, now))
    }

    fn refill(&self, bucket: Bucket, now: Instant) -> f64 {
        let elapsed = now.saturating_duration_since(bucket.at).as_secs_f64();
        (bucket.tokens + elapsed * self.per_minute / 60.0).min(self.per_minute)
    }

    fn take(&mut self, key: i64, now: Instant) {
        let tokens = self.tokens(key, now) - 1.0;
        self.buckets.insert(key, Bucket { tokens, at: now });
    }

    /// Drops full buckets, which behave the same as missing ones.
    fn prune(&mut self, now: Instant) {
        if self.last_prune.is_some_and(|at| now.saturating_duration_since(at) < Duration::from_secs(60)) {
            return;
        }
        self.last_prune = Some(now);
        let per_minute = self.per_minute;
        let refill = |b: &Bucket| b.tokens + now.saturating_duration_since(b.at).as_secs_f64() * per_minute / 60.0;
        self.buckets.retain(|_, b| refill(b) < per_minute);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Limited {
    User,
    Chat,
}

#[derive(Debug)]
pub struct Limits {
    users: Limiter,
    chats: Limiter,
}

impl Limits {
    pub fn new(user_per_minute: u32, chat_per_minute: u32) -> Self {
        Self { users: Limiter::new(user_per_minute), chats: Limiter::new(chat_per_minute) }
    }

    /// Takes one reply from both budgets, or neither if either is empty.
    pub fn acquire(&mut self, user: i64, chat: i64, now: Instant) -> Result<(), Limited> {
        self.users.prune(now);
        self.chats.prune(now);
        if self.users.tokens(user, now) < 1.0 {
            return Err(Limited::User);
        }
        if self.chats.tokens(chat, now) < 1.0 {
            return Err(Limited::Chat);
        }
        self.users.take(user, now);
        self.chats.take(chat, now);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_limit_then_refill() {
        let mut limits = Limits::new(3, 100);
        let t = Instant::now();
        for _ in 0..3 {
            assert_eq!(limits.acquire(1, -1, t), Ok(()));
        }
        assert_eq!(limits.acquire(1, -1, t), Err(Limited::User));
        assert_eq!(limits.acquire(2, -1, t), Ok(()), "other users aren't affected");
        // One token every 20 s at 3 a minute.
        assert_eq!(limits.acquire(1, -1, t + Duration::from_secs(19)), Err(Limited::User));
        assert_eq!(limits.acquire(1, -1, t + Duration::from_secs(20)), Ok(()));
    }

    #[test]
    fn chat_limit_doesnt_spend_user_budget() {
        let mut limits = Limits::new(2, 2);
        let t = Instant::now();
        assert_eq!(limits.acquire(1, -1, t), Ok(()));
        assert_eq!(limits.acquire(2, -1, t), Ok(()));
        assert_eq!(limits.acquire(3, -1, t), Err(Limited::Chat));
        assert_eq!(limits.acquire(3, -2, t), Ok(()));
        assert_eq!(limits.acquire(3, -2, t), Ok(()), "the refused attempt didn't use a token");
    }

    #[test]
    fn prunes_full_buckets() {
        let mut limits = Limits::new(10, 10);
        let t = Instant::now();
        limits.acquire(1, -1, t).unwrap();
        limits.acquire(2, -2, t + Duration::from_secs(61)).unwrap();
        limits.acquire(3, -3, t + Duration::from_secs(122)).unwrap();
        assert!(!limits.users.buckets.contains_key(&1));
        assert!(limits.users.buckets.contains_key(&3));
    }
}
