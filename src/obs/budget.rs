//! The RPC budget. Kizzy's request budget, carried over with three lanes.
//!
//! One token bucket paces every request. The lanes differ in how much of
//! the bucket they must leave behind: the copy path may drain it, the
//! watcher leaves a fifth, forensics leaves half. So a long forensic job
//! can never make a copy wait. Forensics also has a daily credit share and
//! is refused once it is spent; the other two lanes are never refused.
//!
//! A 429 stands everything down for the `Retry-After`, cuts the rate, and
//! the rate climbs back one step per clean minute.

use std::collections::VecDeque;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::time::Instant;

use crate::util::{Clock, day_of};

const RATE_LIMIT_BACKOFF_S: f64 = 5.0;
const MAX_RATE_LIMIT_BACKOFF_S: f64 = 120.0;
const BAN_MARGIN_S: f64 = 1.0;
const BUDGET_CUT: f64 = 0.7;
const MIN_REQUESTS_PER_MIN: f64 = 30.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lane {
    /// The path from a seen buy to a copy. Never starved.
    Copy,
    /// Wallet streams, tripwires and the backfill.
    Watch,
    /// Trail walks, hop windows, maps and reviews.
    Forensics,
}

impl Lane {
    pub const ALL: [Lane; 3] = [Lane::Copy, Lane::Watch, Lane::Forensics];

    pub fn as_str(&self) -> &'static str {
        match self {
            Lane::Copy => "copy",
            Lane::Watch => "watch",
            Lane::Forensics => "forensics",
        }
    }

    fn index(&self) -> usize {
        match self {
            Lane::Copy => 0,
            Lane::Watch => 1,
            Lane::Forensics => 2,
        }
    }

    /// The share of the bucket this lane must leave for the lanes above it.
    fn reserve(&self) -> f64 {
        match self {
            Lane::Copy => 0.0,
            Lane::Watch => 0.2,
            Lane::Forensics => 0.5,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BudgetConfig {
    pub requests_per_min: f64,
    pub burst: f64,
    /// What the provider plan allows in a day, in credits.
    pub daily_credits: i64,
    /// The part of the day's credits forensics may use.
    pub forensics_share: f64,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        BudgetConfig { requests_per_min: 600.0, burst: 40.0, daily_credits: 300_000, forensics_share: 0.3 }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BudgetError {
    /// Forensics has used its share of today's credits.
    ForensicsSpent { spent: i64, cap: i64 },
}

impl std::fmt::Display for BudgetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BudgetError::ForensicsSpent { spent, cap } => write!(f, "forensics has used {spent} of its {cap} credits today"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BudgetSnapshot {
    pub rate_per_min: f64,
    pub max_rate: f64,
    pub banned_for_s: f64,
    pub requests: u64,
    pub rate_limits: u64,
    pub day: String,
    /// Credits spent today, by lane, in `Lane::ALL` order.
    pub spent: [i64; 3],
    pub daily_credits: i64,
    pub forensics_cap: i64,
}

impl BudgetSnapshot {
    pub fn total(&self) -> i64 {
        self.spent.iter().sum()
    }
}

struct State {
    max_rate: f64,
    rate_per_min: f64,
    burst: f64,
    tokens: f64,
    refilled_at: Instant,
    sent: VecDeque<Instant>,
    banned_until: Option<Instant>,
    ban_step_s: f64,
    changed_at: Option<Instant>,
    requests: u64,
    rate_limits: u64,
    day: String,
    spent: [i64; 3],
    unflushed: [i64; 3],
    warned_over: bool,
}

pub struct Budget {
    config: BudgetConfig,
    clock: Clock,
    state: Mutex<State>,
}

impl Budget {
    pub fn new(config: BudgetConfig, clock: Clock) -> Self {
        let max_rate = config.requests_per_min.max(1.0);
        let burst = config.burst.max(1.0);
        let day = day_of(clock());
        Budget {
            state: Mutex::new(State {
                max_rate,
                rate_per_min: max_rate,
                burst,
                tokens: burst,
                refilled_at: Instant::now(),
                sent: VecDeque::new(),
                banned_until: None,
                ban_step_s: RATE_LIMIT_BACKOFF_S,
                changed_at: None,
                requests: 0,
                rate_limits: 0,
                day,
                spent: [0; 3],
                unflushed: [0; 3],
                warned_over: false,
            }),
            config,
            clock,
        }
    }

    fn forensics_cap(&self) -> i64 {
        (self.config.daily_credits as f64 * self.config.forensics_share.clamp(0.0, 1.0)) as i64
    }

    /// A new UTC day starts the credit count again.
    fn roll_day(&self, state: &mut State) {
        let today = day_of((self.clock)());
        if state.day != today {
            state.day = today;
            state.spent = [0; 3];
            state.warned_over = false;
        }
    }

    /// Waits until this lane may send one request costing `credits`, and
    /// counts it. Only forensics can be refused.
    pub async fn acquire(&self, lane: Lane, credits: i64) -> Result<(), BudgetError> {
        loop {
            let wait = {
                let mut state = self.state.lock();
                self.roll_day(&mut state);
                if lane == Lane::Forensics {
                    let cap = self.forensics_cap();
                    let total: i64 = state.spent.iter().sum();
                    if state.spent[lane.index()] + credits > cap || total + credits > self.config.daily_credits {
                        return Err(BudgetError::ForensicsSpent { spent: state.spent[lane.index()], cap });
                    }
                }
                let now = Instant::now();
                if let Some(until) = state.banned_until.filter(|until| *until > now) {
                    until - now
                } else {
                    let elapsed = now.duration_since(state.refilled_at).as_secs_f64();
                    state.tokens = (state.tokens + elapsed * state.rate_per_min / 60.0).min(state.burst);
                    state.refilled_at = now;
                    let needed = 1.0 + state.burst * lane.reserve();
                    if state.tokens >= needed {
                        state.tokens -= 1.0;
                        state.requests += 1;
                        state.spent[lane.index()] += credits;
                        state.unflushed[lane.index()] += credits;
                        state.sent.push_back(now);
                        while state.sent.front().is_some_and(|at| now.duration_since(*at).as_secs_f64() > 60.0) {
                            state.sent.pop_front();
                        }
                        let total: i64 = state.spent.iter().sum();
                        if total > self.config.daily_credits && !state.warned_over {
                            state.warned_over = true;
                            log::warn!("today's RPC credits ({total}) passed the daily budget of {}", self.config.daily_credits);
                        }
                        return Ok(());
                    }
                    Duration::from_secs_f64(((needed - state.tokens) * 60.0 / state.rate_per_min).max(0.001))
                }
            };
            tokio::time::sleep(wait).await;
        }
    }

    /// The provider answered 429. `retry_after_s` is its `Retry-After`, or 0.
    pub fn on_rate_limited(&self, retry_after_s: f64) {
        let mut state = self.state.lock();
        let now = Instant::now();
        state.rate_limits += 1;
        let pause = if retry_after_s > 0.0 {
            retry_after_s + BAN_MARGIN_S
        } else {
            let pause = state.ban_step_s;
            state.ban_step_s = (state.ban_step_s * 2.0).min(MAX_RATE_LIMIT_BACKOFF_S);
            pause
        };
        // Requests already in flight when the ban began come back 429 too;
        // they are the same ban, not another reason to cut.
        let fresh = state.banned_until.is_none_or(|until| now >= until);
        let until = now + Duration::from_secs_f64(pause);
        state.banned_until = Some(state.banned_until.map_or(until, |current| current.max(until)));
        if !fresh {
            return;
        }
        let spent = state.sent.iter().filter(|at| now.duration_since(**at).as_secs_f64() <= 60.0).count() as f64;
        state.rate_per_min = (state.rate_per_min.min(spent.max(1.0)) * BUDGET_CUT).max(MIN_REQUESTS_PER_MIN.min(state.max_rate));
        state.changed_at = Some(now);
        log::warn!("RPC rate limited after {spent:.0} requests in a minute; standing down {pause:.0}s, then {:.0} req/min", state.rate_per_min);
    }

    /// A request succeeded. After a clean minute the rate climbs one step.
    pub fn on_ok(&self) {
        let mut state = self.state.lock();
        state.ban_step_s = RATE_LIMIT_BACKOFF_S;
        if state.rate_per_min >= state.max_rate {
            return;
        }
        let now = Instant::now();
        let since = [state.changed_at, state.banned_until].into_iter().flatten().max();
        if since.is_none_or(|at| now >= at && now.duration_since(at).as_secs_f64() >= 60.0) {
            let step = (state.max_rate / 10.0).max(1.0);
            state.rate_per_min = (state.rate_per_min + step).min(state.max_rate);
            state.changed_at = Some(now);
        }
    }

    pub fn snapshot(&self) -> BudgetSnapshot {
        let mut state = self.state.lock();
        self.roll_day(&mut state);
        let now = Instant::now();
        BudgetSnapshot {
            rate_per_min: state.rate_per_min,
            max_rate: state.max_rate,
            banned_for_s: state.banned_until.filter(|until| *until > now).map(|until| (until - now).as_secs_f64()).unwrap_or(0.0),
            requests: state.requests,
            rate_limits: state.rate_limits,
            day: state.day.clone(),
            spent: state.spent,
            daily_credits: self.config.daily_credits,
            forensics_cap: self.forensics_cap(),
        }
    }

    /// Credits counted since the last call, for the ledger: `(day, lane, credits)`.
    pub fn take_unflushed(&self) -> Vec<(String, Lane, i64)> {
        let mut state = self.state.lock();
        let day = state.day.clone();
        let mut out = Vec::new();
        for lane in Lane::ALL {
            let credits = std::mem::take(&mut state.unflushed[lane.index()]);
            if credits > 0 {
                out.push((day.clone(), lane, credits));
            }
        }
        out
    }

    /// Loads what the ledger says was already spent today, at start-up.
    pub fn restore(&self, day: &str, lane: Lane, credits: i64) {
        let mut state = self.state.lock();
        self.roll_day(&mut state);
        if state.day == day {
            state.spent[lane.index()] += credits;
        }
    }

    /// For tests: empties the bucket so the next request has to wait.
    pub fn drain(&self) {
        let mut state = self.state.lock();
        state.tokens = 0.0;
        state.refilled_at = Instant::now();
    }
}
