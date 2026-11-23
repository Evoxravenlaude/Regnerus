//! Solana transaction bytes, with no SDK: read a serialized legacy or v0
//! transaction, check that it is the swap that was asked for, and sign it.
//! Ported from Yeks.
//!
//! Wire format:
//!   [compact-u16 signature count] [64 bytes each] [message]
//! Message:
//!   v0 only: a prefix byte with the high bit set
//!   header: required signatures, read-only signed, read-only unsigned (one byte each)
//!   [compact-u16 n] [32-byte account keys]   (key 0 pays the fee; the first keys sign)
//!   [32-byte recent blockhash]
//!   [compact-u16 n] instructions: program index, [compact-u16 n][account indexes], [compact-u16 n][data]
//!   v0 only: [compact-u16 n] lookup tables: 32-byte key, [compact-u16 n][writable], [compact-u16 n][read-only]

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use curve25519_dalek::edwards::CompressedEdwardsY;
use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};

use crate::chain::solana::tx::{COMPUTE_BUDGET_PROGRAM, SYSTEM_PROGRAM, TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID, WSOL_MINT};

pub const ASSOCIATED_TOKEN_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
pub const JUPITER_PROGRAM: &str = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
/// Lamports the wallet may send to anything other than its own wrapped-SOL
/// account inside a swap: rent and tips. 0.01 SOL.
pub const SYSTEM_OUTFLOW_CEILING: u64 = 10_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxError(pub String);

impl std::fmt::Display for TxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn err<T>(text: impl Into<String>) -> Result<T, TxError> {
    Err(TxError(text.into()))
}

/// A trading key. The secret never leaves this struct except through `secret_base58`.
pub struct Keypair(SigningKey);

impl Keypair {
    pub fn generate() -> Result<Keypair, TxError> {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| TxError(format!("no randomness: {e}")))?;
        Ok(Keypair(SigningKey::from_bytes(&seed)))
    }

    /// Reads a secret as wallets export it: base58 of 64 bytes (seed then
    /// public key), base58 of the 32-byte seed, or a JSON array of 64 numbers.
    pub fn from_secret(text: &str) -> Result<Keypair, TxError> {
        let text = text.trim();
        let bytes: Vec<u8> = if text.starts_with('[') {
            serde_json::from_str::<Vec<u8>>(text).map_err(|_| TxError("the key is not a list of bytes".into()))?
        } else {
            bs58::decode(text).into_vec().map_err(|_| TxError("the key is not base58".into()))?
        };
        let seed: [u8; 32] = match bytes.len() {
            32 | 64 => bytes[..32].try_into().map_err(|_| TxError("bad key".into()))?,
            other => return err(format!("a key is 32 or 64 bytes, this is {other}")),
        };
        let key = Keypair(SigningKey::from_bytes(&seed));
        if bytes.len() == 64 && bytes[32..] != key.public_bytes() {
            return err("the key's two halves do not belong together");
        }
        Ok(key)
    }

    pub fn public_bytes(&self) -> [u8; 32] {
        self.0.verifying_key().to_bytes()
    }

    pub fn address(&self) -> String {
        bs58::encode(self.public_bytes()).into_string()
    }

    /// Seed then public key, base58: the form wallets import.
    pub fn secret_base58(&self) -> String {
        let mut bytes = self.0.to_bytes().to_vec();
        bytes.extend_from_slice(&self.public_bytes());
        bs58::encode(bytes).into_string()
    }

    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        self.0.sign(message).to_bytes()
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> Result<u8, TxError> {
        let byte = *self.bytes.get(self.at).ok_or_else(|| TxError("truncated".into()))?;
        self.at += 1;
        Ok(byte)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], TxError> {
        let end = self.at.checked_add(n).filter(|end| *end <= self.bytes.len()).ok_or_else(|| TxError("truncated".into()))?;
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }

    /// One to three bytes, seven bits each, low group first.
    fn compact(&mut self) -> Result<usize, TxError> {
        let mut value = 0usize;
        for i in 0..3 {
            let byte = self.u8()?;
            value |= ((byte & 0x7f) as usize) << (7 * i);
            if byte & 0x80 == 0 {
                if i > 0 && byte == 0 {
                    return err("non-canonical length");
                }
                return if value > 0xffff { err("length overflow") } else { Ok(value) };
            }
        }
        err("length longer than 3 bytes")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompiledInstruction {
    pub program_index: u8,
    pub accounts: Vec<u8>,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedTransaction {
    pub signature_count: usize,
    /// Where the message starts in the serialized bytes.
    pub message_offset: usize,
    pub versioned: bool,
    pub required_signatures: usize,
    pub static_keys: Vec<[u8; 32]>,
    pub instructions: Vec<CompiledInstruction>,
}

impl ParsedTransaction {
    pub fn key(&self, index: u8) -> Option<String> {
        self.static_keys.get(index as usize).map(|k| bs58::encode(k).into_string())
    }

    pub fn fee_payer(&self) -> String {
        self.key(0).unwrap_or_default()
    }
}

/// Reads a serialized transaction. Anything malformed, and any trailing byte, is an error.
pub fn parse_transaction(bytes: &[u8]) -> Result<ParsedTransaction, TxError> {
    let mut r = Reader { bytes, at: 0 };
    let signature_count = r.compact()?;
    r.take(64 * signature_count)?;
    let message_offset = r.at;
    let first = *bytes.get(r.at).ok_or_else(|| TxError("empty message".into()))?;
    let versioned = first & 0x80 != 0;
    if versioned {
        let version = r.u8()? & 0x7f;
        if version != 0 {
            return err(format!("unsupported message version {version}"));
        }
    }
    let required_signatures = r.u8()? as usize;
    let readonly_signed = r.u8()? as usize;
    let readonly_unsigned = r.u8()? as usize;
    let key_count = r.compact()?;
    let mut static_keys = Vec::with_capacity(key_count);
    for _ in 0..key_count {
        static_keys.push(r.take(32)?.try_into().map_err(|_| TxError("bad key".into()))?);
    }
    if required_signatures == 0 || required_signatures > key_count {
        return err("bad signer count");
    }
    if readonly_signed >= required_signatures || readonly_unsigned > key_count - required_signatures {
        return err("bad read-only counts");
    }
    if signature_count != required_signatures {
        return err(format!("{signature_count} signature slots for {required_signatures} signers"));
    }
    r.take(32)?; // recent blockhash
    let instruction_count = r.compact()?;
    let mut instructions = Vec::with_capacity(instruction_count);
    for _ in 0..instruction_count {
        let program_index = r.u8()?;
        let n = r.compact()?;
        let accounts = r.take(n)?.to_vec();
        let n = r.compact()?;
        let data = r.take(n)?.to_vec();
        instructions.push(CompiledInstruction { program_index, accounts, data });
    }
    if versioned {
        for _ in 0..r.compact()? {
            r.take(32)?;
            let n = r.compact()?;
            r.take(n)?;
            let n = r.compact()?;
            r.take(n)?;
        }
    }
    if r.at != bytes.len() {
        return err(format!("{} trailing bytes", bytes.len() - r.at));
    }
    Ok(ParsedTransaction { signature_count, message_offset, versioned, required_signatures, static_keys, instructions })
}

pub fn decode_base64(text: &str) -> Result<Vec<u8>, TxError> {
    B64.decode(text.trim()).map_err(|_| TxError("the transaction is not base64".into()))
}

pub fn encode_base64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

/// Signs the message with our key and writes the signature into our slot
/// only. Returns the signed bytes and the transaction id (slot 0), base58.
pub fn sign_transaction(bytes: &[u8], key: &Keypair) -> Result<(Vec<u8>, String), TxError> {
    let parsed = parse_transaction(bytes)?;
    let ours = key.public_bytes();
    let Some(slot) = parsed.static_keys.iter().take(parsed.required_signatures).position(|k| *k == ours) else {
        return err(format!("wallet {} is not a signer of this transaction (fee payer {})", key.address(), parsed.fee_payer()));
    };
    let signature = key.sign(&bytes[parsed.message_offset..]);
    let mut out = bytes.to_vec();
    let prefix = parsed.message_offset - 64 * parsed.signature_count;
    out[prefix + 64 * slot..prefix + 64 * (slot + 1)].copy_from_slice(&signature);
    let id = bs58::encode(&out[prefix..prefix + 64]).into_string();
    Ok((out, id))
}

fn on_curve(bytes: &[u8; 32]) -> bool {
    CompressedEdwardsY(*bytes).decompress().is_some()
}

/// The first bump, from 255 down, whose hash is off the curve.
pub fn find_program_address(seeds: &[&[u8]], program: &[u8; 32]) -> Option<[u8; 32]> {
    for bump in (0..=255u8).rev() {
        let mut hasher = Sha256::new();
        for seed in seeds {
            hasher.update(seed);
        }
        hasher.update([bump]);
        hasher.update(program);
        hasher.update(b"ProgramDerivedAddress");
        let address: [u8; 32] = hasher.finalize().into();
        if !on_curve(&address) {
            return Some(address);
        }
    }
    None
}

fn key_bytes(address: &str) -> Option<[u8; 32]> {
    bs58::decode(address).into_vec().ok()?.try_into().ok()
}

/// The associated token account of `owner` for `mint`.
pub fn associated_token_address(owner: &str, mint: &str, token_program: &str) -> Option<String> {
    let (owner, mint, token, ata) = (key_bytes(owner)?, key_bytes(mint)?, key_bytes(token_program)?, key_bytes(ASSOCIATED_TOKEN_PROGRAM)?);
    find_program_address(&[&owner, &token, &mint], &ata).map(|a| bs58::encode(a).into_string())
}

fn u64_at(data: &[u8], offset: usize) -> Option<u64> {
    data.get(offset..offset + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap_or_default()))
}

/// Refuses a swap transaction unless it is shaped like one built for this
/// wallet and nobody else:
///   - one required signature, and it is ours (we pay the fee);
///   - every top-level instruction calls an allowed program;
///   - no token Approve, ApproveChecked or SetAuthority;
///   - System transfers funded by us: at most the quoted SOL into our own
///     wrapped-SOL account, and at most the ceiling to anything else.
///
/// `Ok` means it passes; `Err` says why not.
pub fn vet_swap(bytes: &[u8], wallet: &str, input_mint: &str, input_amount: u64, extra_programs: &[String]) -> Result<(), String> {
    let tx = parse_transaction(bytes).map_err(|e| format!("unreadable transaction ({e})"))?;
    if tx.required_signatures != 1 {
        return Err(format!("it asks for {} signatures (expected only ours)", tx.required_signatures));
    }
    if tx.fee_payer() != wallet {
        return Err(format!("the fee payer is {}, not this wallet", tx.fee_payer()));
    }
    let allowed = |program: &str| {
        matches!(program, COMPUTE_BUDGET_PROGRAM | SYSTEM_PROGRAM | TOKEN_PROGRAM_ID | TOKEN_2022_PROGRAM_ID | ASSOCIATED_TOKEN_PROGRAM | JUPITER_PROGRAM)
            || extra_programs.iter().any(|p| p == program)
    };
    let wsol_account = associated_token_address(wallet, WSOL_MINT, TOKEN_PROGRAM_ID).unwrap_or_default();
    let (mut to_wsol, mut to_others) = (0u64, 0u64);
    for (i, ix) in tx.instructions.iter().enumerate() {
        // A program id must be a static key; one that points into a lookup table is refused.
        let Some(program) = tx.key(ix.program_index) else { return Err(format!("instruction {i} takes its program from a lookup table")) };
        if !allowed(&program) {
            return Err(format!("instruction {i} calls {program}, which is not an allowed swap program"));
        }
        if program == TOKEN_PROGRAM_ID || program == TOKEN_2022_PROGRAM_ID {
            match ix.data.first() {
                Some(4 | 13) => return Err(format!("instruction {i} would approve a delegate on a token account")),
                Some(6) => return Err(format!("instruction {i} would change the authority of a token account")),
                _ => {}
            }
        }
        if program != SYSTEM_PROGRAM {
            continue;
        }
        let kind = ix.data.get(..4).map(|b| u32::from_le_bytes(b.try_into().unwrap_or_default()));
        let lamports = match kind {
            // Transfer and CreateAccount: [from, to], lamports after the tag.
            Some(0 | 2) => u64_at(&ix.data, 4),
            // CreateAccountWithSeed: lamports after the base key and the seed.
            Some(3) => u64_at(&ix.data, 36).and_then(|len| u64_at(&ix.data, 44 + len as usize)),
            // Assign, Allocate and the with-seed transfers move nothing of ours.
            Some(1 | 8 | 9 | 10 | 11 | 13) => continue,
            other => return Err(format!("instruction {i} is a System instruction ({other:?}) a swap does not need")),
        };
        let from = ix.accounts.first().and_then(|a| tx.key(*a));
        if from.as_deref() != Some(wallet) {
            continue;
        }
        let Some(lamports) = lamports else { return Err(format!("instruction {i}: malformed System instruction")) };
        if ix.accounts.get(1).and_then(|a| tx.key(*a)).as_deref() == Some(wsol_account.as_str()) {
            to_wsol = to_wsol.saturating_add(lamports);
        } else {
            to_others = to_others.saturating_add(lamports);
        }
    }
    let quoted = if input_mint == WSOL_MINT { input_amount } else { 0 };
    if to_others > SYSTEM_OUTFLOW_CEILING {
        return Err(format!("it would send {to_others} lamports from this wallet to other accounts"));
    }
    if to_wsol > quoted.saturating_add(SYSTEM_OUTFLOW_CEILING) {
        return Err(format!("it would wrap {to_wsol} lamports, more than the {quoted} quoted"));
    }
    Ok(())
}
