//! The arithmetic that turns evidence into a wallet's state. Pure: rows in,
//! a verdict out. The points are starting values; `/weights` replaces them
//! with measured ones.

use std::collections::BTreeMap;

use crate::model::{Evidence, LinkedBy, WalletState};

pub const CANDIDATE_AT: i32 = 40;
pub const LINK_AT: i32 = 70;
/// The review band: scores the scripts cannot settle alone.
pub const GREY_FROM: i32 = 50;

pub mod kind {
    pub const ROOT: &str = "root";
    pub const ADMIN_LINK: &str = "admin_link";
    pub const ADMIN_CONFIRM: &str = "admin_confirm";
    pub const DEPOSIT_PROOF: &str = "deposit_proof";
    pub const TOKEN_HANDOFF: &str = "token_handoff";
    pub const DIRECT_FUNDING: &str = "direct_funding";
    pub const FUNDED_LINKED: &str = "funded_linked";
    pub const SHARED_SINK: &str = "shared_sink";
    pub const CEX_HOP_TIGHT: &str = "cex_hop_tight";
    pub const CEX_HOP_LOOSE: &str = "cex_hop_loose";
    pub const CEX_HOP_WEAK: &str = "cex_hop_weak";
    pub const HABIT: &str = "habit";
    pub const COBUY_SLOTS: &str = "cobuy_slots";
    pub const COBUY_MINUTE: &str = "cobuy_minute";
    pub const ROTATION: &str = "rotation";
    pub const FINGERPRINT: &str = "fingerprint";
    pub const OTHER_DEPOSIT: &str = "other_deposit";
    pub const AI_REVIEW: &str = "ai_review";
}

pub mod family {
    pub const MANUAL: &str = "manual";
    pub const PROOF: &str = "proof";
    pub const FUNDING: &str = "funding";
    pub const HABIT: &str = "habit";
    pub const COBUY: &str = "cobuy";
    pub const TIMING: &str = "timing";
    pub const FINGERPRINT: &str = "fingerprint";
    pub const NEGATIVE: &str = "negative";
    pub const REVIEW: &str = "review";
}

/// `(kind, family, starting points, what it means)`.
pub const KINDS: &[(&str, &str, i32, &str)] = &[
    (kind::DEPOSIT_PROOF, family::PROOF, 100, "paid into the same exchange deposit address"),
    (kind::TOKEN_HANDOFF, family::FUNDING, 60, "received tokens a linked wallet bought, and sold them"),
    (kind::DIRECT_FUNDING, family::FUNDING, 45, "first SOL came from a linked wallet"),
    (kind::FUNDED_LINKED, family::FUNDING, 45, "it gave a linked wallet its first SOL"),
    (kind::SHARED_SINK, family::FUNDING, 35, "proceeds go to the same plain wallet"),
    (kind::CEX_HOP_TIGHT, family::FUNDING, 40, "exchange withdrawal matching a deposit closely"),
    (kind::CEX_HOP_LOOSE, family::FUNDING, 20, "exchange withdrawal roughly matching a deposit"),
    (kind::CEX_HOP_WEAK, family::FUNDING, 5, "one of several withdrawals that fit a deposit"),
    (kind::HABIT, family::HABIT, 10, "same exchange and amount band as the entity's funding"),
    (kind::COBUY_SLOTS, family::COBUY, 12, "bought a thinly bought token within 3 slots"),
    (kind::COBUY_MINUTE, family::COBUY, 6, "bought a thinly bought token within 60 seconds"),
    (kind::ROTATION, family::TIMING, 8, "an old wallet went quiet as this one started"),
    (kind::FINGERPRINT, family::FINGERPRINT, 15, "builds transactions the same way"),
    (kind::OTHER_DEPOSIT, family::NEGATIVE, -10, "uses a different deposit address at the same exchange"),
    (kind::AI_REVIEW, family::REVIEW, 15, "the reviewer's verdict"),
];

pub fn family_of(kind_name: &str) -> &'static str {
    match kind_name {
        kind::ROOT | kind::ADMIN_LINK | kind::ADMIN_CONFIRM => family::MANUAL,
        other => KINDS.iter().find(|(k, ..)| *k == other).map(|(_, f, ..)| *f).unwrap_or(family::NEGATIVE),
    }
}

pub fn describe(kind_name: &str) -> &'static str {
    match kind_name {
        kind::ROOT => "the wallet this entity was started from",
        kind::ADMIN_LINK | kind::ADMIN_CONFIRM => "an admin said so",
        other => KINDS.iter().find(|(k, ..)| *k == other).map(|(.., text)| *text).unwrap_or("evidence"),
    }
}

/// The most a family can add, however many rows it has.
pub fn family_cap(family_name: &str) -> Option<i32> {
    match family_name {
        family::COBUY => Some(36),
        family::FINGERPRINT => Some(15),
        family::HABIT => Some(10),
        family::TIMING => Some(8),
        family::REVIEW => Some(15),
        _ => None,
    }
}

/// Current points per kind: the starting values with any approved changes laid over them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Weights(pub BTreeMap<String, i32>);

impl Weights {
    pub fn points(&self, kind_name: &str) -> i32 {
        self.0.get(kind_name).copied().unwrap_or_else(|| KINDS.iter().find(|(k, ..)| *k == kind_name).map(|(_, _, p, _)| *p).unwrap_or(0))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub score: i32,
    pub state: WalletState,
    pub linked_by: Option<LinkedBy>,
    /// Points per family after caps, for showing the arithmetic.
    pub families: BTreeMap<String, i32>,
    pub funding: bool,
}

impl Verdict {
    /// Whether the trader may act on this wallet's trades without an admin:
    /// proof and manual links always, score links once calibration has
    /// enough proofs, review links never.
    pub fn trusted(&self, calibrated: bool) -> bool {
        match self.linked_by {
            Some(LinkedBy::Root | LinkedBy::Proof | LinkedBy::Admin) => true,
            Some(LinkedBy::Score) => calibrated,
            _ => false,
        }
    }
}

/// Reads the live rows tying one wallet to one entity. `was_rejected` is an
/// earlier "not them": only an admin can undo it.
pub fn judge(rows: &[Evidence], was_rejected: bool) -> Verdict {
    let mut families: BTreeMap<String, i32> = BTreeMap::new();
    let mut manual: Option<LinkedBy> = None;
    let mut proof = false;
    for row in rows.iter().filter(|r| !r.voided) {
        match row.kind.as_str() {
            kind::ROOT => manual = Some(LinkedBy::Root),
            kind::ADMIN_LINK | kind::ADMIN_CONFIRM => manual = manual.or(Some(LinkedBy::Admin)),
            kind::DEPOSIT_PROOF => proof = true,
            _ => *families.entry(family_of(&row.kind).to_string()).or_insert(0) += row.points,
        }
    }
    for (name, points) in families.iter_mut() {
        if let Some(cap) = family_cap(name) {
            *points = (*points).min(cap);
        }
        if name == family::REVIEW {
            *points = (*points).max(-25);
        }
    }
    let review = families.get(family::REVIEW).copied().unwrap_or(0);
    let funding = families.get(family::FUNDING).copied().unwrap_or(0) > 0;
    let base: i32 = families.iter().filter(|(name, _)| name.as_str() != family::REVIEW).map(|(_, p)| *p).sum();
    let score = (base + review).clamp(0, 100);

    let (state, linked_by, score) = if let Some(by) = manual {
        (WalletState::Linked, Some(by), 100)
    } else if was_rejected {
        (WalletState::Rejected, None, 0)
    } else if proof {
        (WalletState::Linked, Some(LinkedBy::Proof), 100)
    } else if base >= LINK_AT && funding {
        (WalletState::Linked, Some(LinkedBy::Score), score.max(LINK_AT))
    } else if score >= LINK_AT && funding {
        // The review carried it over the line: cards only until something else agrees.
        (WalletState::Linked, Some(LinkedBy::Review), score)
    } else if score >= CANDIDATE_AT {
        // Behaviour alone never links, whatever it adds up to.
        (WalletState::Candidate, None, if funding { score } else { score.min(LINK_AT - 1) })
    } else {
        (WalletState::Seen, None, score)
    };
    Verdict { score, state, linked_by, families, funding }
}
