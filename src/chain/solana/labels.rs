//! The label book: which addresses are services and not people.
//!
//! It starts from a small seed that is safe to ship (Jito's published tip
//! accounts). Everything else is added by an admin with `/label`, or loaded
//! from the database at start-up. No trading-app fee wallet is seeded: those
//! addresses have to be captured from real transactions first.

use std::collections::HashMap;

use parking_lot::RwLock;

use crate::model::{Label, LabelClass};

/// Jito's mainnet tip accounts, as published in Jito's docs
/// (`getTipAccounts`).
pub const JITO_TIP_ACCOUNTS: &[&str] = &[
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];

#[derive(Default)]
pub struct LabelBook {
    map: RwLock<HashMap<String, Label>>,
}

impl LabelBook {
    pub fn empty() -> Self {
        LabelBook::default()
    }

    pub fn seeded() -> Self {
        let book = LabelBook::default();
        for address in JITO_TIP_ACCOUNTS {
            book.set(Label { address: (*address).to_string(), class: LabelClass::Tip, name: "jito".to_string(), source: "seed".to_string() });
        }
        book
    }

    pub fn get(&self, address: &str) -> Option<Label> {
        self.map.read().get(address).cloned()
    }

    /// Adds or replaces. A later label wins, so an admin can correct a seed.
    pub fn set(&self, label: Label) {
        self.map.write().insert(label.address.clone(), label);
    }

    pub fn remove(&self, address: &str) -> bool {
        self.map.write().remove(address).is_some()
    }

    pub fn len(&self) -> usize {
        self.map.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.read().is_empty()
    }

    /// Every label, sorted by class then name, for `/labels`.
    pub fn all(&self) -> Vec<Label> {
        let mut out: Vec<Label> = self.map.read().values().cloned().collect();
        out.sort_by(|a, b| (a.class.as_str(), &a.name, &a.address).cmp(&(b.class.as_str(), &b.name, &b.address)));
        out
    }

    pub fn is_class(&self, address: &str, class: LabelClass) -> bool {
        self.map.read().get(address).is_some_and(|l| l.class == class)
    }
}
