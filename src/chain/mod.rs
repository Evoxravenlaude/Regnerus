//! The chain seam. Identity, signals and the bot see only these types; the
//! Solana adapter lives underneath, and an EVM adapter can sit beside it.

pub mod solana;
