//! Entry point. `regnerus` runs the watcher; the other subcommands are
//! tools for the measuring phase and for debugging one transaction.

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::watch;

use regnerus::app::App;
use regnerus::bot::telegram::{TelegramCall, TelegramHttp};
use regnerus::chain::solana::labels::LabelBook;
use regnerus::chain::solana::rpc::{ChainRpc, SolanaRpc};
use regnerus::chain::solana::stream::AddressSource;
use regnerus::chain::solana::tx::{ParsedTx, classify, fingerprint};
use regnerus::config::Settings;
use regnerus::logger;
use regnerus::obs::budget::{Budget, BudgetConfig, Lane};
use regnerus::runtime::{serve, tell_admins};
use regnerus::store::Store;
use regnerus::store::memory::MemoryStore;
use regnerus::store::pg::PgStore;
use regnerus::util::{is_signature, is_solana_address, system_clock};

const USAGE: &str = "\
regnerus                         run the bot
regnerus migrate                 apply database migrations and exit
regnerus capture SIGNATURE [NAME]
                                  save a transaction as fixtures/NAME.json
regnerus classify SIGNATURE WALLET
                                  show how a transaction is read for a wallet
Settings come from the environment or ./.env; see .env.example.";

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn budget_of(settings: &Settings) -> Arc<Budget> {
    Arc::new(Budget::new(
        BudgetConfig { requests_per_min: settings.rpc_requests_per_min, burst: settings.rpc_burst, daily_credits: settings.rpc_daily_credits, forensics_share: settings.forensics_share },
        system_clock(),
    ))
}

async fn open_store(settings: &Settings) -> Result<Arc<dyn Store>, String> {
    if settings.database_url.is_empty() {
        log::warn!("DATABASE_URL is not set: everything is kept in memory and lost on restart");
        return Ok(Arc::new(MemoryStore::new()));
    }
    let store = PgStore::connect(&settings.database_url, &settings.db_schema).await.map_err(|e| format!("could not connect to the database: {e}"))?;
    let applied = store.migrate().await.map_err(|e| format!("migration failed: {e}"))?;
    if !applied.is_empty() {
        log::info!("applied migrations {applied:?} in schema {}", settings.db_schema);
    }
    Ok(Arc::new(store))
}

/// Labels kept in a file beside the binary: exchanges, bridges, app wallets.
/// `[{"address": "...", "class": "cex_hot", "name": "Binance"}]`
fn seed_labels(app: &App) -> usize {
    let path = std::env::var("LABELS_FILE").unwrap_or_else(|_| "labels.json".to_string());
    let Ok(text) = std::fs::read_to_string(&path) else { return 0 };
    let rows: Vec<serde_json::Value> = match serde_json::from_str(&text) {
        Ok(rows) => rows,
        Err(error) => {
            log::warn!("{path} is not a JSON list of labels: {error}");
            return 0;
        }
    };
    let mut loaded = 0;
    for row in rows {
        let field = |key: &str| row.get(key).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        match (regnerus::util::is_solana_address(&field("address")), regnerus::model::LabelClass::parse(&field("class"))) {
            (true, Some(class)) => {
                app.labels.set(regnerus::model::Label { address: field("address"), class, name: field("name"), source: "seed".to_string() });
                loaded += 1;
            }
            _ => log::warn!("{path}: skipped a label that is not an address with a known class: {row}"),
        }
    }
    loaded
}

/// Gives the app its swap venue, key provider and reviewer, as configured.
fn equip(app: &Arc<App>) -> Result<(), String> {
    let settings = &app.settings;
    if settings.jupiter_url != "off" {
        let venue = regnerus::trade::venue::JupiterVenue::new(&settings.jupiter_url, &settings.jupiter_api_key, &settings.jupiter_exclude_routers)
            .map_err(|e| format!("could not build the swap client: {e}"))?;
        app.set_venue(Arc::new(venue));
    }
    if settings.secrets_provider == "doppler" {
        let keys = regnerus::trade::keys::DopplerKeys::new(&settings.doppler_token, &settings.doppler_write_token, &settings.doppler_project, &settings.doppler_config)?;
        app.set_keys(Arc::new(keys));
    } else {
        app.set_keys(Arc::new(regnerus::trade::keys::EnvKeys));
    }
    if !settings.anthropic_api_key.is_empty() {
        let reviewer =
            regnerus::review::AnthropicReviewer::new(&settings.anthropic_url, &settings.anthropic_api_key, &settings.review_model).map_err(|e| format!("could not build the reviewer client: {e}"))?;
        app.set_reviewer(Arc::new(reviewer));
    }
    Ok(())
}

async fn run(settings: Settings) -> ExitCode {
    let problems = settings.problems();
    if !problems.is_empty() {
        for problem in problems {
            eprintln!("{problem}");
        }
        eprintln!("On Railway: open the service, Variables. Locally: copy .env.example to .env and fill it in.");
        return ExitCode::FAILURE;
    }
    let store = match open_store(&settings).await {
        Ok(store) => store,
        Err(error) => {
            log::error!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let budget = budget_of(&settings);
    let rpc = match SolanaRpc::new(&settings.solana_rpc_url, settings.commitment, budget.clone()) {
        Ok(rpc) => rpc,
        Err(error) => {
            log::error!("could not build the RPC client: {error}");
            return ExitCode::FAILURE;
        }
    };
    let rpc_host = rpc.host().to_string();
    let tg: Arc<dyn TelegramCall> = match TelegramHttp::new(&settings.telegram_bot_token, settings.dry_run) {
        Ok(tg) => Arc::new(tg),
        Err(error) => {
            log::error!("could not build the Telegram client: {error}");
            return ExitCode::FAILURE;
        }
    };
    let rpc: Arc<dyn ChainRpc> = Arc::new(rpc);
    let app = App::new(settings.clone(), store, rpc, tg, Arc::new(LabelBook::seeded()), budget, system_clock(), rpc_host.clone());
    let seeded = seed_labels(&app);
    let labels = app.load_labels().await;
    app.restore_budget().await;
    if let Err(error) = equip(&app) {
        log::error!("{error}");
        return ExitCode::FAILURE;
    }
    let watched = app.addresses().await.len();
    log::info!("starting: {watched} addresses to watch, {labels} stored labels, {seeded} from the labels file, RPC {rpc_host}, commitment {}", settings.commitment.as_str());
    log::info!(
        "trader: {} · venue {} · keys {} · reviewer {}",
        if settings.is_live() { "LIVE" } else { "paper only" },
        app.venue().map(|v| v.name()).unwrap_or_else(|| "none".into()),
        app.keys().map(|k| k.name()).unwrap_or("none"),
        app.reviewer().map(|r| r.model()).unwrap_or_else(|| "off".into())
    );

    let (stop, stopped) = watch::channel(false);
    let tasks = tokio::spawn(serve(app.clone(), stopped));

    tell_admins(
        &app,
        &format!("Regnerus started, watching {}. Trading is {}. Send /help.", regnerus::util::count(watched, "address", "addresses"), if settings.is_live() { "LIVE" } else { "on paper" }),
    )
    .await;
    shutdown_signal().await;
    log::info!("stopping");
    let _ = stop.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(25), tasks).await;
    app.flush_ledger().await;
    log::info!("stopped after {} events and {} trades", app.health.events.load(Ordering::Relaxed), app.health.trades.load(Ordering::Relaxed));
    ExitCode::SUCCESS
}

async fn migrate(settings: Settings) -> ExitCode {
    if settings.database_url.is_empty() {
        eprintln!("DATABASE_URL is not set.");
        return ExitCode::FAILURE;
    }
    let store = match PgStore::connect(&settings.database_url, &settings.db_schema).await {
        Ok(store) => store,
        Err(error) => {
            eprintln!("could not connect: {error}");
            return ExitCode::FAILURE;
        }
    };
    match store.migrate().await {
        Ok(applied) => {
            println!("schema {}: applied {applied:?}", settings.db_schema);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

/// The RPC client alone, for the two tool commands.
fn tool_rpc(settings: &Settings) -> Result<SolanaRpc, String> {
    if settings.solana_rpc_url.is_empty() {
        return Err("SOLANA_RPC_URL is not set.".to_string());
    }
    SolanaRpc::new(&settings.solana_rpc_url, settings.commitment, budget_of(settings))
}

async fn fetch(settings: &Settings, signature: &str) -> Result<serde_json::Value, String> {
    if !is_signature(signature) {
        return Err(format!("{signature} is not a transaction signature."));
    }
    let rpc = tool_rpc(settings)?;
    match rpc.get_transaction(signature, Lane::Watch).await {
        Ok(Some(tx)) => Ok(tx),
        Ok(None) => Err("the node does not have that transaction (too old for this endpoint, or not finalized yet).".to_string()),
        Err(error) => Err(error.to_string()),
    }
}

/// Saves one real transaction as a test fixture.
async fn capture(settings: Settings, args: &[String]) -> ExitCode {
    let Some(signature) = args.first() else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };
    let name: String = args.get(1).cloned().unwrap_or_else(|| signature.chars().take(12).collect());
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        eprintln!("NAME may hold letters, digits, - and _ only.");
        return ExitCode::FAILURE;
    }
    let tx = match fetch(&settings, signature).await {
        Ok(tx) => tx,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let path = format!("fixtures/{name}.json");
    let text = serde_json::to_string_pretty(&tx).unwrap_or_default();
    if let Err(error) = std::fs::create_dir_all("fixtures").and_then(|_| std::fs::write(&path, text.as_bytes())) {
        eprintln!("could not write {path}: {error}");
        return ExitCode::FAILURE;
    }
    let parsed = ParsedTx::from_value(&tx);
    let keys = parsed.as_ref().map(|p| p.transaction.message.account_keys.len()).unwrap_or(0);
    println!("saved {path}: {} bytes, slot {}, {keys} accounts", text.len(), parsed.map(|p| p.slot).unwrap_or(0));
    ExitCode::SUCCESS
}

/// Shows what the classifier makes of one transaction for one wallet.
async fn classify_one(settings: Settings, args: &[String]) -> ExitCode {
    let (Some(signature), Some(wallet)) = (args.first(), args.get(1)) else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };
    if !is_solana_address(wallet) {
        eprintln!("{wallet} is not a Solana address.");
        return ExitCode::FAILURE;
    }
    let tx = match fetch(&settings, signature).await {
        Ok(tx) => tx,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let Some(parsed) = ParsedTx::from_value(&tx) else {
        eprintln!("the transaction did not parse.");
        return ExitCode::FAILURE;
    };
    let labels = LabelBook::seeded();
    println!("{:#?}", classify(&parsed, wallet));
    println!("{:#?}", fingerprint(&parsed, wallet, &|a| labels.get(a)));
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let settings = Settings::from_env();
    logger::init(&settings.log_level);
    // One TLS provider for HTTP, the websocket and Postgres.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let runtime = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("could not start the runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(async {
        match args.first().map(String::as_str) {
            None | Some("run") => run(settings).await,
            Some("migrate") => migrate(settings).await,
            Some("capture") => capture(settings, &args[1..]).await,
            Some("classify") => classify_one(settings, &args[1..]).await,
            _ => {
                println!("{USAGE}");
                ExitCode::SUCCESS
            }
        }
    })
}
