//! The running bot: the stream, the workers that handle its signatures, the
//! backfill and ledger timers, and the Telegram poller. `main` builds the
//! `App` and calls `serve`; a test can do the same against local servers.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::sync::{Semaphore, mpsc, watch};

use crate::app::App;
use crate::bot::telegram::{Card, LONG_POLL_S, send};
use crate::chain::solana::stream::{AddressSource, StreamConfig, StreamMsg, run_stream};
use crate::util::ago;

/// Stream outages shorter than this are routine and not announced.
const DOWN_NOTICE_AFTER_S: i64 = 120;
/// How many signatures are handled at once.
const WORKERS: usize = 16;

pub async fn tell_admins(app: &App, text: &str) {
    for admin in &app.settings.admin_ids {
        send(app.tg.as_ref(), *admin, &Card::text(text)).await;
    }
}

/// Long-polls Telegram and hands each update to the app, in order.
pub async fn poll_updates(app: Arc<App>, mut shutdown: watch::Receiver<bool>) {
    // A leftover webhook would make getUpdates fail.
    app.tg.call("deleteWebhook", json!({}), false).await;
    let mut offset: i64 = 0;
    loop {
        let payload = json!({ "offset": offset, "timeout": LONG_POLL_S, "allowed_updates": ["message", "callback_query"] });
        let reply = tokio::select! {
            reply = app.tg.call("getUpdates", payload, true) => reply,
            _ = shutdown.changed() => return,
        };
        if !reply.ok {
            log::warn!("getUpdates failed: {}", reply.error);
            tokio::select! { _ = tokio::time::sleep(Duration::from_secs(3)) => {}, _ = shutdown.changed() => return }
            continue;
        }
        for update in reply.result.as_array().into_iter().flatten() {
            if let Some(id) = update.get("update_id").and_then(serde_json::Value::as_i64) {
                offset = offset.max(id + 1);
            }
            app.on_update(update).await;
        }
    }
}

/// Runs every background task until `stopped` turns true.
pub async fn serve(app: Arc<App>, stopped: watch::Receiver<bool>) {
    let settings = &app.settings;
    let (events_in, mut events) = mpsc::channel::<StreamMsg>(1_024);

    let stream = tokio::spawn(run_stream(
        StreamConfig { ws_url: settings.solana_ws_url.clone(), commitment: settings.commitment.as_str().to_string() },
        app.clone() as Arc<dyn AddressSource>,
        events_in,
        app.health.clone(),
        app.clock.clone(),
        app.refresh.clone(),
        stopped.clone(),
    ));

    // Signatures are handled side by side, so one slow lookup holds nothing up.
    let workers = Arc::new(Semaphore::new(WORKERS));
    let consumer = {
        let app = app.clone();
        tokio::spawn(async move {
            while let Some(msg) = events.recv().await {
                let Ok(permit) = workers.clone().acquire_owned().await else { return };
                let app = app.clone();
                tokio::spawn(async move {
                    app.on_stream(msg).await;
                    drop(permit);
                });
            }
        })
    };

    let housekeeping = {
        let app = app.clone();
        let mut stopped = stopped.clone();
        tokio::spawn(async move {
            let mut backfill = tokio::time::interval(Duration::from_secs(app.settings.backfill_interval_s));
            let mut minute = tokio::time::interval(Duration::from_secs(60));
            backfill.tick().await; // the connect already ran a pass
            minute.tick().await;
            let mut announced_down = false;
            loop {
                tokio::select! {
                    _ = backfill.tick() => {
                        let handled = app.backfill_all().await;
                        if handled > 0 {
                            log::info!("backfill pass handled {handled} transactions the stream had not delivered");
                        }
                    }
                    _ = minute.tick() => {
                        app.flush_ledger().await;
                        match app.stream_down_for() {
                            Some(down) if down >= DOWN_NOTICE_AFTER_S && !announced_down => {
                                announced_down = true;
                                tell_admins(&app, &format!("The wallet stream has been down for {}. Reconnecting; missed trades are read on reconnect.", ago(down))).await;
                            }
                            None if announced_down => {
                                announced_down = false;
                                tell_admins(&app, "The wallet stream is back.").await;
                            }
                            _ => {}
                        }
                    }
                    _ = stopped.changed() => return,
                }
            }
        })
    };

    let poller = tokio::spawn(poll_updates(app.clone(), stopped.clone()));

    // Forensics runs at its own pace: one job at a time, on its own lane.
    let forensics = {
        let app = app.clone();
        let mut stopped = stopped.clone();
        tokio::spawn(async move {
            loop {
                let pause = if app.run_next_job().await { Duration::from_millis(200) } else { Duration::from_secs(app.settings.forensics_idle_s) };
                tokio::select! { _ = tokio::time::sleep(pause) => {}, _ = stopped.changed() => return }
            }
        })
    };

    // Open positions are marked to market and their exit rules applied.
    let monitor = {
        let app = app.clone();
        let mut stopped = stopped.clone();
        tokio::spawn(async move {
            let mut pass = tokio::time::interval(Duration::from_secs(app.settings.monitor_interval_s));
            let mut slow = tokio::time::interval(Duration::from_secs(6 * 3_600));
            pass.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = pass.tick() => {
                        app.monitor_positions().await;
                    }
                    _ = slow.tick() => {
                        if let Err(error) = app.copyability_pass().await {
                            log::warn!("copyability pass failed: {error}");
                        }
                        if let Err(error) = app.lost_check().await {
                            log::warn!("lost check failed: {error}");
                        }
                    }
                    _ = stopped.changed() => return,
                }
            }
        })
    };

    let _ = stream.await;
    let _ = housekeeping.await;
    let _ = poller.await;
    // A job or a monitor pass in flight is allowed to finish: a sent order must be recorded.
    let _ = forensics.await;
    let _ = monitor.await;
    consumer.abort();
}
