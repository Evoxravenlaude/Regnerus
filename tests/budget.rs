//! The RPC budget under a paused clock: lane headroom, the forensics share,
//! standing down on a 429, and the ledger.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use regnerus::obs::budget::{Budget, BudgetConfig, BudgetError, Lane};
use regnerus::util::Clock;
use tokio::time::Instant;

fn fixed_clock(at: i64) -> (Clock, Arc<AtomicI64>) {
    let now = Arc::new(AtomicI64::new(at));
    let handle = now.clone();
    (Arc::new(move || handle.load(Ordering::Relaxed)), now)
}

const DAY1: i64 = 1_795_348_800; // 2026-11-22 00:00:00 UTC

fn budget(requests_per_min: f64, burst: f64, daily: i64) -> (Budget, Arc<AtomicI64>) {
    let (clock, now) = fixed_clock(DAY1 + 3_600);
    (Budget::new(BudgetConfig { requests_per_min, burst, daily_credits: daily, forensics_share: 0.3 }, clock), now)
}

/// How long an acquire had to wait.
async fn waited(budget: &Budget, lane: Lane) -> Duration {
    let start = Instant::now();
    budget.acquire(lane, 1).await.expect("not refused");
    start.elapsed()
}

#[tokio::test(start_paused = true)]
async fn lower_lanes_leave_headroom_for_the_copy_path() {
    let (budget, _) = budget(60.0, 10.0, 1_000_000);
    // Forensics may take the bucket from 10 down to 5 and no further.
    for _ in 0..5 {
        assert!(waited(&budget, Lane::Forensics).await < Duration::from_millis(1));
    }
    let blocked = tokio::time::timeout(Duration::from_millis(500), budget.acquire(Lane::Forensics, 1)).await;
    assert!(blocked.is_err(), "the sixth forensics request has to wait for a refill");
    // The watcher may go on down to 2, and the copy path to 0, with no wait.
    for _ in 0..3 {
        assert!(waited(&budget, Lane::Watch).await < Duration::from_millis(1));
    }
    let blocked = tokio::time::timeout(Duration::from_millis(100), budget.acquire(Lane::Watch, 1)).await;
    assert!(blocked.is_err(), "the watcher leaves a fifth of the bucket");
    for _ in 0..2 {
        assert!(waited(&budget, Lane::Copy).await < Duration::from_millis(1), "a copy never waits behind the other lanes");
    }
    // Empty now: at 60 a minute the next copy waits about a second.
    let wait = waited(&budget, Lane::Copy).await;
    assert!(wait >= Duration::from_millis(300) && wait <= Duration::from_millis(1_100), "{wait:?}");
}

#[tokio::test(start_paused = true)]
async fn forensics_is_refused_once_its_daily_share_is_spent() {
    let (budget, now) = budget(6_000.0, 100.0, 100);
    assert_eq!(budget.snapshot().forensics_cap, 30);
    budget.acquire(Lane::Forensics, 20).await.unwrap();
    assert_eq!(budget.acquire(Lane::Forensics, 20).await, Err(BudgetError::ForensicsSpent { spent: 20, cap: 30 }));
    budget.acquire(Lane::Forensics, 10).await.unwrap();
    // The other lanes are never refused, even past the day's budget.
    budget.acquire(Lane::Watch, 80).await.unwrap();
    budget.acquire(Lane::Copy, 5).await.unwrap();
    let snap = budget.snapshot();
    assert_eq!(snap.spent, [5, 80, 30]);
    assert_eq!(snap.total(), 115);

    // A new UTC day starts the count again.
    now.store(DAY1 + 86_400 + 60, Ordering::Relaxed);
    let snap = budget.snapshot();
    assert_eq!((snap.day.as_str(), snap.total()), ("2026-11-23", 0));
    budget.acquire(Lane::Forensics, 30).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn forensics_cannot_push_the_day_past_its_budget() {
    let (budget, _) = budget(6_000.0, 100.0, 100);
    budget.acquire(Lane::Watch, 95).await.unwrap();
    assert!(budget.acquire(Lane::Forensics, 10).await.is_err(), "10 more would pass the day's 100");
    budget.acquire(Lane::Forensics, 5).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_429_stands_everything_down_then_the_rate_climbs_back() {
    let (budget, _) = budget(600.0, 40.0, 1_000_000);
    for _ in 0..10 {
        budget.acquire(Lane::Watch, 1).await.unwrap();
    }
    budget.on_rate_limited(10.0);
    let snap = budget.snapshot();
    assert!(snap.banned_for_s > 10.0 && snap.banned_for_s <= 11.0, "Retry-After plus a margin: {}", snap.banned_for_s);
    assert_eq!(snap.rate_per_min, 30.0, "cut to 70% of what was being sent, floored at 30 a minute");
    assert_eq!(snap.rate_limits, 1);

    // A second 429 from a request already in flight is the same ban.
    budget.on_rate_limited(10.0);
    assert_eq!(budget.snapshot().rate_per_min, 30.0);

    // Even the copy lane waits out the ban.
    let wait = waited(&budget, Lane::Copy).await;
    assert!(wait >= Duration::from_secs(10), "{wait:?}");

    // No climb until a clean minute has passed since the ban ended.
    budget.on_ok();
    assert_eq!(budget.snapshot().rate_per_min, 30.0);
    tokio::time::sleep(Duration::from_secs(61)).await;
    budget.on_ok();
    assert_eq!(budget.snapshot().rate_per_min, 90.0, "one step of a tenth of the full rate");
    budget.on_ok();
    assert_eq!(budget.snapshot().rate_per_min, 90.0, "one step per clean minute, not per request");
    for _ in 0..12 {
        tokio::time::sleep(Duration::from_secs(61)).await;
        budget.on_ok();
    }
    assert_eq!(budget.snapshot().rate_per_min, 600.0, "never above the configured rate");
}

#[tokio::test(start_paused = true)]
async fn a_429_without_retry_after_backs_off_in_doubling_steps() {
    let (budget, _) = budget(600.0, 40.0, 1_000_000);
    budget.on_rate_limited(0.0);
    assert!((budget.snapshot().banned_for_s - 5.0).abs() < 0.01);
    tokio::time::sleep(Duration::from_secs(6)).await;
    budget.on_rate_limited(0.0);
    assert!((budget.snapshot().banned_for_s - 10.0).abs() < 0.01);
    // A success resets the step.
    tokio::time::sleep(Duration::from_secs(11)).await;
    budget.on_ok();
    budget.on_rate_limited(0.0);
    assert!((budget.snapshot().banned_for_s - 5.0).abs() < 0.01);
}

#[tokio::test(start_paused = true)]
async fn the_ledger_gets_each_credit_once_and_restores_on_restart() {
    let (budget, _) = budget(6_000.0, 100.0, 1_000);
    budget.acquire(Lane::Watch, 3).await.unwrap();
    budget.acquire(Lane::Watch, 2).await.unwrap();
    budget.acquire(Lane::Copy, 1).await.unwrap();
    let flushed = budget.take_unflushed();
    assert_eq!(flushed, vec![("2026-11-22".to_string(), Lane::Copy, 1), ("2026-11-22".to_string(), Lane::Watch, 5)]);
    assert!(budget.take_unflushed().is_empty(), "nothing is flushed twice");

    // A restarted process reads today's ledger back; another day's rows are ignored.
    let (fresh, _) = self::budget(6_000.0, 100.0, 1_000);
    fresh.restore("2026-11-22", Lane::Forensics, 290);
    fresh.restore("2026-11-21", Lane::Watch, 999);
    assert_eq!(fresh.snapshot().spent, [0, 0, 290]);
    assert!(fresh.acquire(Lane::Forensics, 20).await.is_err(), "the share spent before the restart still counts");
    assert!(fresh.take_unflushed().is_empty(), "restored credits are not written to the ledger again");
}
