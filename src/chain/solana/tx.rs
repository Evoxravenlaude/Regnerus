//! Pure classification of one Solana transaction (`getTransaction`,
//! `jsonParsed`) for one watched wallet. Ported from Yeks's
//! `classifySolanaTx`, with transfers and the fee fingerprint added.
//!
//! No program decoding decides the side: the balance diffs are the truth.
//! Exactly one non-quote mint moving against a quote (SOL, native or
//! wrapped, USDC or USDT) is a trade. Anything else is looked at as a set
//! of transfers, and what is neither is `Other`.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::Deserialize;
use serde_json::Value;

use crate::model::{Direction, Fingerprint, Label, LabelClass, Quote, Side, TokenProgram, Venue};

pub const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";
pub const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
pub const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
pub const USDT_MINT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";
pub const TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const TOKEN_2022_PROGRAM_ID: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
pub const COMPUTE_BUDGET_PROGRAM: &str = "ComputeBudget111111111111111111111111111111";

/// Rent-exempt minimum of a 165-byte token account: what a wallet pays to
/// open one and gets back when it closes one.
pub const TOKEN_ACCOUNT_RENT_LAMPORTS: i128 = 2_039_280;
/// Below these the quote leg is rent or fee noise, not a trade.
pub const MIN_QUOTE_LAMPORTS: i128 = 5_000_000; // 0.005 SOL
pub const MIN_QUOTE_STABLE_RAW: i128 = 500_000; // $0.50
/// SOL transfers below this are dust and address-poisoning spam.
pub const MIN_TRANSFER_LAMPORTS: u64 = 100_000; // 0.0001 SOL
/// The network's base fee per signature.
pub const BASE_FEE_LAMPORTS: u64 = 5_000;

/// Well-known programs and the venue each one means. Taken from Yeks.
pub const VENUE_PROGRAMS: &[(&str, Venue)] = &[
    ("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P", Venue::PumpFun),  // pump.fun bonding curve
    ("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA", Venue::PumpSwap), // PumpSwap AMM
    ("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8", Venue::Raydium), // Raydium AMM v4
    ("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK", Venue::Raydium), // Raydium CLMM
    ("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C", Venue::Raydium), // Raydium CPMM
    ("LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj", Venue::Raydium),  // Raydium LaunchLab
    ("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4", Venue::Jupiter),  // Jupiter v6
    ("JUP4Fb2cqiRUcaTHdrPC8h2gNsA2ETXiPDD33WcGuJB", Venue::Jupiter),  // Jupiter v4
    ("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo", Venue::Meteora),  // Meteora DLMM
    ("Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB", Venue::Meteora), // Meteora dynamic pools
    ("dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN", Venue::Meteora),  // Meteora dynamic bonding curve
    ("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc", Venue::Orca),     // Orca Whirlpools
];

/// Plumbing every transaction touches; dropped from `programs` so the list
/// says where the trade happened.
const PLUMBING: &[&str] = &[
    SYSTEM_PROGRAM,
    TOKEN_PROGRAM_ID,
    TOKEN_2022_PROGRAM_ID,
    "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
    COMPUTE_BUDGET_PROGRAM,
    "SysvarRent111111111111111111111111111111111",
    "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr",
];

/// The aggregator when one routed the trade, else the first known DEX.
const VENUE_ORDER: &[Venue] = &[Venue::Jupiter, Venue::PumpFun, Venue::PumpSwap, Venue::Raydium, Venue::Meteora, Venue::Orca];

// ---- getTransaction (jsonParsed) shape: only what is read ----

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(default)]
pub struct UiTokenAmount {
    pub amount: String,
    pub decimals: u8,
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct TokenBalance {
    pub account_index: usize,
    pub mint: String,
    pub owner: Option<String>,
    pub program_id: Option<String>,
    pub ui_token_amount: UiTokenAmount,
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Instruction {
    pub program_id: String,
    pub parsed: Option<Value>,
    pub accounts: Vec<String>,
    pub data: Option<String>,
}

/// `jsonParsed` gives objects; plain `json` gives strings. Both are read.
#[derive(Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum AccountKey {
    Full {
        pubkey: String,
        #[serde(default)]
        signer: bool,
    },
    Plain(String),
}

impl AccountKey {
    pub fn pubkey(&self) -> &str {
        match self {
            AccountKey::Full { pubkey, .. } => pubkey,
            AccountKey::Plain(pubkey) => pubkey,
        }
    }

    pub fn signer(&self) -> bool {
        matches!(self, AccountKey::Full { signer: true, .. })
    }
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Message {
    pub account_keys: Vec<AccountKey>,
    pub instructions: Vec<Instruction>,
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(default)]
pub struct TransactionBody {
    pub signatures: Vec<String>,
    pub message: Message,
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(default)]
pub struct InnerInstructions {
    pub index: usize,
    pub instructions: Vec<Instruction>,
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Meta {
    pub err: Option<Value>,
    pub fee: u64,
    pub pre_balances: Vec<u64>,
    pub post_balances: Vec<u64>,
    pub pre_token_balances: Option<Vec<TokenBalance>>,
    pub post_token_balances: Option<Vec<TokenBalance>>,
    pub inner_instructions: Option<Vec<InnerInstructions>>,
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ParsedTx {
    pub slot: u64,
    pub block_time: Option<i64>,
    pub transaction: TransactionBody,
    pub meta: Option<Meta>,
}

impl ParsedTx {
    /// Reads the `result` of `getTransaction`. `None` when it is not one.
    pub fn from_value(value: &Value) -> Option<ParsedTx> {
        if !value.is_object() {
            return None;
        }
        serde_json::from_value(value.clone()).ok()
    }

    pub fn signature(&self) -> Option<&str> {
        self.transaction.signatures.first().map(String::as_str)
    }

    pub fn failed(&self) -> bool {
        self.meta.as_ref().is_none_or(|m| m.err.is_some())
    }

    fn key(&self, index: usize) -> Option<&str> {
        self.transaction.message.account_keys.get(index).map(AccountKey::pubkey)
    }

    /// Every instruction, top level first, then inner ones.
    fn all_instructions(&self) -> impl Iterator<Item = &Instruction> {
        let inner = self.meta.as_ref().and_then(|m| m.inner_instructions.as_ref());
        self.transaction.message.instructions.iter().chain(inner.into_iter().flat_map(|groups| groups.iter().flat_map(|g| g.instructions.iter())))
    }
}

/// The trade fields the classifier can read from the transaction alone.
#[derive(Clone, Debug, PartialEq)]
pub struct TradeCore {
    pub signature: String,
    pub slot: u64,
    pub block_time: Option<i64>,
    pub side: Side,
    pub mint: String,
    pub token_amount_raw: u64,
    pub decimals: u8,
    pub token_program: Option<TokenProgram>,
    pub quote: Quote,
    pub quote_amount_raw: u64,
    pub programs: Vec<String>,
    pub venue: Venue,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferCore {
    pub direction: Direction,
    pub counterparty: String,
    pub mint: Option<String>,
    pub amount_raw: u64,
    pub decimals: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Classified {
    Trade(TradeCore),
    Transfers(Vec<TransferCore>),
    Other,
}

/// Program ids the transaction invoked, outermost first, plumbing removed.
pub fn programs_of(tx: &ParsedTx) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for ix in tx.all_instructions() {
        let id = ix.program_id.as_str();
        if !id.is_empty() && !PLUMBING.contains(&id) && !out.iter().any(|p| p == id) {
            out.push(id.to_string());
        }
    }
    out
}

pub fn venue_of(programs: &[String]) -> Venue {
    let hit: Vec<Venue> = programs.iter().filter_map(|p| VENUE_PROGRAMS.iter().find(|(id, _)| id == p).map(|(_, v)| *v)).collect();
    VENUE_ORDER.iter().copied().find(|v| hit.contains(v)).unwrap_or(Venue::Other)
}

fn quote_of_mint(mint: &str) -> Option<Quote> {
    match mint {
        WSOL_MINT => Some(Quote::Sol),
        USDC_MINT => Some(Quote::Usdc),
        USDT_MINT => Some(Quote::Usdt),
        _ => None,
    }
}

#[derive(Default)]
struct MintAcc {
    pre: i128,
    post: i128,
    decimals: u8,
    program_id: Option<String>,
    pre_accounts: BTreeSet<usize>,
    post_accounts: BTreeSet<usize>,
}

/// One wallet's trade in a transaction. `None` when it is not a trade, when
/// the transaction failed, or when the wallet is not among its accounts.
pub fn classify_trade(tx: &ParsedTx, wallet: &str) -> Option<TradeCore> {
    let meta = tx.meta.as_ref()?;
    if meta.err.is_some() {
        return None;
    }
    let keys = &tx.transaction.message.account_keys;
    let idx = keys.iter().position(|k| k.pubkey() == wallet)?;
    let signature = tx.signature()?.to_string();

    // Native SOL, net of the fee when the wallet paid it. The fee payer is
    // always account 0.
    let pre = *meta.pre_balances.get(idx).unwrap_or(&0) as i128;
    let post = *meta.post_balances.get(idx).unwrap_or(&0) as i128;
    let mut sol_delta = post - pre + if idx == 0 { meta.fee as i128 } else { 0 };

    // Token balances the wallet owns, summed per mint across its accounts.
    let mut by_mint: BTreeMap<String, MintAcc> = BTreeMap::new();
    let mut touch = |balance: &TokenBalance, is_pre: bool| {
        if balance.owner.as_deref() != Some(wallet) || balance.mint.is_empty() {
            return;
        }
        let acc = by_mint.entry(balance.mint.clone()).or_insert_with(|| MintAcc { decimals: balance.ui_token_amount.decimals, program_id: balance.program_id.clone(), ..MintAcc::default() });
        let amount: i128 = balance.ui_token_amount.amount.parse().unwrap_or(0);
        if is_pre {
            acc.pre += amount;
            acc.pre_accounts.insert(balance.account_index);
        } else {
            acc.post += amount;
            acc.post_accounts.insert(balance.account_index);
        }
        if acc.program_id.is_none() {
            acc.program_id = balance.program_id.clone();
        }
    };
    for balance in meta.pre_token_balances.iter().flatten() {
        touch(balance, true);
    }
    for balance in meta.post_token_balances.iter().flatten() {
        touch(balance, false);
    }

    // Wrapped SOL is SOL. Stables are quotes. Everything else is an asset.
    let mut usdc_delta: i128 = 0;
    let mut usdt_delta: i128 = 0;
    let mut assets: Vec<(&String, i128, &MintAcc)> = Vec::new();
    let mut opened: i128 = 0;
    let mut closed: i128 = 0;
    for (mint, acc) in &by_mint {
        let delta = acc.post - acc.pre;
        match quote_of_mint(mint) {
            Some(Quote::Sol) => {
                sol_delta += delta;
                continue;
            }
            Some(Quote::Usdc) => {
                usdc_delta += delta;
                continue;
            }
            Some(Quote::Usdt) => {
                usdt_delta += delta;
                continue;
            }
            None => {}
        }
        if delta == 0 {
            continue;
        }
        assets.push((mint, delta, acc));
        opened += acc.post_accounts.difference(&acc.pre_accounts).count() as i128;
        closed += acc.pre_accounts.difference(&acc.post_accounts).count() as i128;
    }
    if assets.len() != 1 {
        return None; // only quotes moved, or a multi-mint route
    }
    let (mint, delta, acc) = assets[0];
    // Opening a token account costs rent the trade did not spend; closing
    // one refunds rent the trade did not earn.
    sol_delta += (opened - closed) * TOKEN_ACCOUNT_RENT_LAMPORTS;

    let side = if delta > 0 { Side::Buy } else { Side::Sell };
    let sign: i128 = if side == Side::Buy { -1 } else { 1 }; // the quote moves the other way
    let candidates = [(Quote::Sol, sol_delta * sign, MIN_QUOTE_LAMPORTS), (Quote::Usdc, usdc_delta * sign, MIN_QUOTE_STABLE_RAW), (Quote::Usdt, usdt_delta * sign, MIN_QUOTE_STABLE_RAW)];
    let mut best: Option<(Quote, i128)> = None;
    for (quote, amount, min) in candidates {
        if amount >= min && amount > best.map(|(_, a)| a).unwrap_or(0) {
            best = Some((quote, amount));
        }
    }
    let (quote, quote_raw) = best?; // token moved with no quote leg: transfer, airdrop or dust

    let programs = programs_of(tx);
    let token_program = match acc.program_id.as_deref() {
        Some(TOKEN_PROGRAM_ID) => Some(TokenProgram::SplToken),
        Some(TOKEN_2022_PROGRAM_ID) => Some(TokenProgram::Token2022),
        _ => None,
    };
    Some(TradeCore {
        signature,
        slot: tx.slot,
        block_time: tx.block_time,
        side,
        mint: mint.clone(),
        token_amount_raw: u64::try_from(delta.unsigned_abs()).unwrap_or(u64::MAX),
        decimals: acc.decimals,
        token_program,
        quote,
        quote_amount_raw: u64::try_from(quote_raw).unwrap_or(u64::MAX),
        venue: venue_of(&programs),
        programs,
    })
}

fn parsed_info(ix: &Instruction) -> Option<(&str, &Value)> {
    let parsed = ix.parsed.as_ref()?;
    Some((parsed.get("type")?.as_str()?, parsed.get("info")?))
}

fn info_str<'a>(info: &'a Value, key: &str) -> Option<&'a str> {
    info.get(key).and_then(Value::as_str)
}

/// A number the node may send as a JSON number or as a string.
fn info_u64(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// One system-program SOL movement: `(source, destination, lamports)`.
fn sol_moves(tx: &ParsedTx) -> Vec<(&str, &str, u64)> {
    let mut out = Vec::new();
    for ix in tx.all_instructions() {
        if ix.program_id != SYSTEM_PROGRAM {
            continue;
        }
        let Some((kind, info)) = parsed_info(ix) else { continue };
        if kind != "transfer" && kind != "transferWithSeed" {
            continue;
        }
        let (Some(source), Some(destination), Some(lamports)) = (info_str(info, "source"), info_str(info, "destination"), info_u64(info.get("lamports"))) else {
            continue;
        };
        out.push((source, destination, lamports));
    }
    out
}

struct TokenAccount<'a> {
    owner: Option<&'a str>,
    mint: &'a str,
    decimals: u8,
}

/// Token accounts the transaction touched, by address.
fn token_accounts(tx: &ParsedTx) -> HashMap<&str, TokenAccount<'_>> {
    let mut out: HashMap<&str, TokenAccount<'_>> = HashMap::new();
    let Some(meta) = tx.meta.as_ref() else { return out };
    for balance in meta.pre_token_balances.iter().flatten().chain(meta.post_token_balances.iter().flatten()) {
        let Some(address) = tx.key(balance.account_index) else { continue };
        out.entry(address).or_insert(TokenAccount { owner: balance.owner.as_deref(), mint: balance.mint.as_str(), decimals: balance.ui_token_amount.decimals });
    }
    out
}

/// What moved between this wallet and other owners: SOL by system transfer,
/// tokens by SPL transfer. Movements between the wallet's own accounts are
/// left out, and so is SOL dust.
pub fn extract_transfers(tx: &ParsedTx, wallet: &str) -> Vec<TransferCore> {
    if tx.failed() {
        return Vec::new();
    }
    // (direction, counterparty, mint) -> (amount, decimals), in a stable order.
    let mut totals: BTreeMap<(u8, String, Option<String>), (u128, u8)> = BTreeMap::new();
    let mut add = |direction: Direction, counterparty: &str, mint: Option<&str>, amount: u64, decimals: u8| {
        let key = (if direction == Direction::In { 0 } else { 1 }, counterparty.to_string(), mint.map(str::to_string));
        let entry = totals.entry(key).or_insert((0, decimals));
        entry.0 += amount as u128;
    };

    for (source, destination, lamports) in sol_moves(tx) {
        if source == destination {
            continue;
        }
        if source == wallet {
            add(Direction::Out, destination, None, lamports, 9);
        } else if destination == wallet {
            add(Direction::In, source, None, lamports, 9);
        }
    }

    let accounts = token_accounts(tx);
    for ix in tx.all_instructions() {
        if ix.program_id != TOKEN_PROGRAM_ID && ix.program_id != TOKEN_2022_PROGRAM_ID {
            continue;
        }
        let Some((kind, info)) = parsed_info(ix) else { continue };
        let amount = match kind {
            "transfer" => info_u64(info.get("amount")),
            "transferChecked" => info_u64(info.get("tokenAmount").and_then(|t| t.get("amount"))),
            _ => continue,
        };
        let (Some(amount), Some(source), Some(destination)) = (amount, info_str(info, "source"), info_str(info, "destination")) else {
            continue;
        };
        if amount == 0 {
            continue;
        }
        let from = accounts.get(source);
        let to = accounts.get(destination);
        let Some(mint) = from.map(|a| a.mint).or(to.map(|a| a.mint)).or_else(|| info_str(info, "mint")) else { continue };
        let decimals = from.map(|a| a.decimals).or(to.map(|a| a.decimals)).unwrap_or(0);
        // The authority signs for the source account, so it stands in for
        // the source's owner when the balances do not name one.
        let from_owner = from.and_then(|a| a.owner).or_else(|| info_str(info, "authority"));
        let to_owner = to.and_then(|a| a.owner);
        if from_owner == Some(wallet) && to_owner != Some(wallet) {
            add(Direction::Out, to_owner.unwrap_or(destination), Some(mint), amount, decimals);
        } else if to_owner == Some(wallet) && from_owner != Some(wallet) {
            add(Direction::In, from_owner.unwrap_or(source), Some(mint), amount, decimals);
        }
    }

    totals
        .into_iter()
        .filter(|((_, _, mint), (amount, _))| mint.is_some() || *amount >= MIN_TRANSFER_LAMPORTS as u128)
        .map(|((direction, counterparty, mint), (amount, decimals))| TransferCore {
            direction: if direction == 0 { Direction::In } else { Direction::Out },
            counterparty,
            mint,
            amount_raw: u64::try_from(amount).unwrap_or(u64::MAX),
            decimals,
        })
        .collect()
}

/// A trade if the wallet traded, else its transfers, else `Other`.
pub fn classify(tx: &ParsedTx, wallet: &str) -> Classified {
    if let Some(trade) = classify_trade(tx, wallet) {
        return Classified::Trade(trade);
    }
    let transfers = extract_transfers(tx, wallet);
    if transfers.is_empty() { Classified::Other } else { Classified::Transfers(transfers) }
}

/// Reads `SetComputeUnitLimit` (2) and `SetComputeUnitPrice` (3) from the
/// compute-budget instructions. Returns `(limit, micro-lamports price)`.
fn compute_budget(tx: &ParsedTx) -> (Option<u32>, Option<u64>) {
    let mut limit = None;
    let mut price = None;
    for ix in &tx.transaction.message.instructions {
        if ix.program_id != COMPUTE_BUDGET_PROGRAM {
            continue;
        }
        if let Some((kind, info)) = parsed_info(ix) {
            match kind {
                "setComputeUnitLimit" => limit = info_u64(info.get("units")).and_then(|v| u32::try_from(v).ok()).or(limit),
                "setComputeUnitPrice" => price = info_u64(info.get("microLamports")).or(price),
                _ => {}
            }
            continue;
        }
        let Some(bytes) = ix.data.as_deref().and_then(|d| bs58::decode(d).into_vec().ok()) else { continue };
        match bytes.as_slice() {
            [2, rest @ ..] if rest.len() >= 4 => limit = Some(u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]])),
            [3, rest @ ..] if rest.len() >= 8 => {
                let mut raw = [0u8; 8];
                raw.copy_from_slice(&rest[..8]);
                price = Some(u64::from_le_bytes(raw));
            }
            _ => {}
        }
    }
    (limit, price)
}

/// How the transaction was built: who paid, the compute-unit settings, the
/// tip and where it went, and which trading app took a fee. `label` looks
/// an address up in the label book.
pub fn fingerprint(tx: &ParsedTx, wallet: &str, label: &dyn Fn(&str) -> Option<Label>) -> Fingerprint {
    let keys = &tx.transaction.message.account_keys;
    let fee_payer = keys.first().map(|k| k.pubkey().to_string()).unwrap_or_default();
    let wallet_signed = keys.iter().any(|k| k.pubkey() == wallet && k.signer());
    let fee = tx.meta.as_ref().map(|m| m.fee).unwrap_or(0);
    let signatures = tx.transaction.signatures.len().max(1) as u64;
    let (cu_limit, cu_price) = compute_budget(tx);

    let mut print =
        Fingerprint { self_paid: fee_payer == wallet, wallet_signed, cu_limit, cu_price, priority_fee_lamports: fee.saturating_sub(BASE_FEE_LAMPORTS * signatures), ..Fingerprint::default() };

    if let Some(found) = label(&fee_payer)
        && found.class == LabelClass::AppSigner
    {
        print.app = Some(found.name);
    }
    for (source, destination, lamports) in sol_moves(tx) {
        if source != wallet && source != fee_payer {
            continue;
        }
        let Some(found) = label(destination) else { continue };
        match found.class {
            LabelClass::Tip => {
                print.tip_lamports += lamports;
                print.tip_account.get_or_insert_with(|| destination.to_string());
                print.tip_family.get_or_insert(found.name);
            }
            LabelClass::AppFee => {
                print.app_fee_lamports += lamports;
                print.app.get_or_insert(found.name);
            }
            _ => {}
        }
    }
    print.fee_payer = fee_payer;
    print
}
