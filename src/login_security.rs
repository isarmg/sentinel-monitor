use crate::{
    auth::verify_password,
    config::Config,
    error::{AppError, Result},
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    hash::Hash,
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{sync::Semaphore, task, time};

const DUMMY_PASSWORD_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$QVY45eyzTvMwT00q1qHjow$t6njtuXI3oRWbaqjK8pyUNyFtckOF2HdosRzSxbZtpk";

#[derive(Clone)]
pub struct LoginProtection {
    limiter: Arc<Mutex<LoginRateLimiter>>,
    argon2_slots: Arc<Semaphore>,
    argon2_timeout: Duration,
}

#[derive(Clone, Copy)]
struct RatePolicy {
    attempts: u32,
    window: Duration,
}

struct LoginRateLimiter {
    sources: BoundedBuckets<IpAddr>,
    accounts: BoundedBuckets<[u8; 32]>,
    source_policy: RatePolicy,
    account_policy: RatePolicy,
}

struct BoundedBuckets<K> {
    entries: HashMap<K, AttemptBucket>,
    capacity: usize,
}

struct AttemptBucket {
    tokens: f64,
    last_refill: Instant,
    last_seen: Instant,
}

impl LoginProtection {
    pub fn new(config: &Config) -> Self {
        Self::from_settings(
            config.login_rate_capacity,
            RatePolicy {
                attempts: config.login_source_attempts,
                window: config.login_source_window,
            },
            RatePolicy {
                attempts: config.login_account_attempts,
                window: config.login_account_window,
            },
            config.login_argon2_parallelism,
            config.login_argon2_timeout,
        )
    }

    pub fn check_attempt(&self, source: IpAddr, account: &str) -> Result<()> {
        let source = normalize_source(source);
        let account = account_digest(account);
        let now = Instant::now();
        let retry = self
            .limiter
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .record(source, account, now);
        if let Some(retry) = retry {
            Err(AppError::RateLimited {
                retry_after: retry_after_seconds(retry),
            })
        } else {
            Ok(())
        }
    }

    pub async fn verify(&self, password: String, encoded: String) -> Result<bool> {
        let slots = self.argon2_slots.clone();
        let retry_after = retry_after_seconds(self.argon2_timeout);
        let verification = time::timeout(self.argon2_timeout, async move {
            let permit = slots
                .acquire_owned()
                .await
                .map_err(|_| AppError::Internal("login verifier unavailable".into()))?;
            task::spawn_blocking(move || {
                let _permit = permit;
                verify_password(&password, &encoded)
            })
            .await
            .map_err(|_| AppError::Internal("login verifier failed".into()))
        })
        .await
        .map_err(|_| AppError::RateLimited { retry_after })??;
        Ok(verification)
    }

    pub fn dummy_password_hash(&self) -> &'static str {
        DUMMY_PASSWORD_HASH
    }

    fn from_settings(
        capacity: usize,
        source_policy: RatePolicy,
        account_policy: RatePolicy,
        argon2_parallelism: usize,
        argon2_timeout: Duration,
    ) -> Self {
        Self {
            limiter: Arc::new(Mutex::new(LoginRateLimiter::new(
                capacity,
                source_policy,
                account_policy,
            ))),
            argon2_slots: Arc::new(Semaphore::new(argon2_parallelism)),
            argon2_timeout,
        }
    }

    #[cfg(test)]
    pub fn for_test(
        capacity: usize,
        source_attempts: u32,
        account_attempts: u32,
        argon2_parallelism: usize,
        argon2_timeout: Duration,
    ) -> Self {
        Self::from_settings(
            capacity,
            RatePolicy {
                attempts: source_attempts,
                window: Duration::from_secs(60),
            },
            RatePolicy {
                attempts: account_attempts,
                window: Duration::from_secs(60),
            },
            argon2_parallelism,
            argon2_timeout,
        )
    }
}

impl LoginRateLimiter {
    fn new(capacity: usize, source_policy: RatePolicy, account_policy: RatePolicy) -> Self {
        Self {
            sources: BoundedBuckets::new(capacity),
            accounts: BoundedBuckets::new(capacity),
            source_policy,
            account_policy,
        }
    }

    fn record(&mut self, source: IpAddr, account: [u8; 32], now: Instant) -> Option<Duration> {
        if let Some(retry) = self.sources.record(source, self.source_policy, now) {
            return Some(retry);
        }
        self.accounts.record(account, self.account_policy, now)
    }
}

impl<K> BoundedBuckets<K>
where
    K: Clone + Eq + Hash,
{
    fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::with_capacity(capacity),
            capacity: capacity.max(1),
        }
    }

    fn record(&mut self, key: K, policy: RatePolicy, now: Instant) -> Option<Duration> {
        if !self.entries.contains_key(&key) && self.entries.len() >= self.capacity {
            self.evict_oldest();
        }
        let bucket = self.entries.entry(key).or_insert(AttemptBucket {
            tokens: f64::from(policy.attempts),
            last_refill: now,
            last_seen: now,
        });
        let refill_rate = f64::from(policy.attempts) / policy.window.as_secs_f64();
        let elapsed = now.saturating_duration_since(bucket.last_refill);
        bucket.tokens =
            (bucket.tokens + elapsed.as_secs_f64() * refill_rate).min(f64::from(policy.attempts));
        bucket.last_refill = now;
        bucket.last_seen = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            None
        } else {
            Some(Duration::from_secs_f64((1.0 - bucket.tokens) / refill_rate))
        }
    }

    fn evict_oldest(&mut self) {
        if let Some(oldest) = self
            .entries
            .iter()
            .min_by_key(|(_, bucket)| bucket.last_seen)
            .map(|(key, _)| key.clone())
        {
            self.entries.remove(&oldest);
        }
    }
}

fn normalize_source(source: IpAddr) -> IpAddr {
    match source {
        IpAddr::V6(source) => source
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(source)),
        source => source,
    }
}

fn account_digest(account: &str) -> [u8; 32] {
    Sha256::digest(account.trim().to_lowercase().as_bytes()).into()
}

fn retry_after_seconds(duration: Duration) -> u64 {
    duration
        .as_secs()
        .saturating_add(u64::from(duration.subsec_nanos() > 0))
        .max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn limiter_enforces_both_dimensions_and_evicts_to_fixed_capacity() {
        let policy = RatePolicy {
            attempts: 2,
            window: Duration::from_secs(60),
        };
        let generous = RatePolicy {
            attempts: 100,
            window: Duration::from_secs(60),
        };
        let now = Instant::now();

        let mut source_limited = LoginRateLimiter::new(8, policy, generous);
        let source = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10));
        assert!(source_limited
            .record(source, account_digest("one@example.com"), now)
            .is_none());
        assert!(source_limited
            .record(source, account_digest("two@example.com"), now)
            .is_none());
        assert!(source_limited
            .record(source, account_digest("three@example.com"), now)
            .is_some());
        assert!(!source_limited
            .accounts
            .entries
            .contains_key(&account_digest("three@example.com")));
        assert!(source_limited
            .record(
                source,
                account_digest("four@example.com"),
                now + Duration::from_secs(30),
            )
            .is_none());

        let mut account_limited = LoginRateLimiter::new(8, generous, policy);
        let account = account_digest("target@example.com");
        assert!(account_limited
            .record(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), account, now)
            .is_none());
        assert!(account_limited
            .record(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2)), account, now)
            .is_none());
        assert!(account_limited
            .record(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 3)), account, now)
            .is_some());

        let mut bounded = LoginRateLimiter::new(2, generous, generous);
        for octet in 1..=10 {
            bounded.record(
                IpAddr::V4(Ipv4Addr::new(198, 51, 100, octet)),
                account_digest(&format!("user-{octet}@example.com")),
                now,
            );
        }
        assert_eq!(bounded.sources.entries.len(), 2);
        assert_eq!(bounded.accounts.entries.len(), 2);

        let IpAddr::V4(source_v4) = source else {
            unreachable!();
        };
        let mapped = IpAddr::V6(source_v4.to_ipv6_mapped());
        assert_eq!(normalize_source(mapped), source);
    }
}
