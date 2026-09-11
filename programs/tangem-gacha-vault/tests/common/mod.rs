//! Shared helpers for the LiteSVM integration suites.
//!
//! Every suite runs the REAL compiled `target/deploy/tangem_gacha_vault.so`.
//! `cargo test` does NOT rebuild that artifact — only `anchor build` /
//! `cargo build-sbf` does. Without a guard, editing `src/lib.rs` and running
//! `cargo test` silently exercises the PREVIOUS binary, so a broken change can
//! come back green and a new assertion can "pass" against code that was never
//! compiled. `program_so_path` fails loudly instead.
//!
//! Shared by all five suites, but not every suite uses every helper (only
//! sweep.rs, core.rs and pnft.rs decode events), so the unused-item lint is
//! off for this module.
#![allow(dead_code)]

// The AnchorDeserialize derive expands to `borsh::...` paths, so the crate
// alias must be in scope here.
use anchor_lang::prelude::borsh;
use base64::Engine;
use std::path::PathBuf;
use std::time::SystemTime;

fn mtime(path: &PathBuf) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// Path to the compiled program, checked for staleness against the sources it
/// is built from. Panics with an actionable message when a rebuild is due.
pub fn program_so_path() -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let so = root.join("../../target/deploy/tangem_gacha_vault.so");

    let so_time = mtime(&so).unwrap_or_else(|| {
        panic!(
            "{} does not exist — run `anchor build` first",
            so.display()
        )
    });

    // Cargo.toml matters too: a dependency or feature change alters the binary
    // without touching lib.rs.
    for src in ["src/lib.rs", "Cargo.toml"] {
        let path = root.join(src);
        if let Some(src_time) = mtime(&path) {
            if src_time > so_time {
                panic!(
                    "target/deploy/tangem_gacha_vault.so is OLDER than {src}.\n\
                     `cargo test` never rebuilds the .so, so this run would test \
                     the previous binary.\n\
                     Run `anchor build` first."
                );
            }
        }
    }

    so.to_string_lossy().into_owned()
}

/// Wire-format pin of the on-chain `BuybackExecuted` event, declared
/// DELIBERATELY independently of the crate's own type (unlike sweep.rs, which
/// imports `PrizeAtaSwept` from the crate): CC's buyback matcher hardcodes
/// this exact layout — discriminator = sha256("event:BuybackExecuted")[..8],
/// then the borsh fields in this order — so a rename, reorder, or retype in
/// lib.rs must fail HERE even though the crate would stay self-consistent.
#[derive(anchor_lang::AnchorDeserialize)]
pub struct BuybackExecuted {
    pub vault: anchor_lang::prelude::Pubkey,
    pub mint: anchor_lang::prelude::Pubkey,
    pub price: u64,
    pub memo: String,
}

impl anchor_lang::Discriminator for BuybackExecuted {
    // sha256("event:BuybackExecuted")[..8]
    const DISCRIMINATOR: &'static [u8] = &[150, 109, 157, 10, 124, 24, 38, 189];
}

/// Decodes the first Anchor event of type `T` out of a transaction's logs.
///
/// Anchor emits events as `Program data: <base64 of discriminator || borsh>`.
/// Nothing in the suite used to decode one, so an event's SHAPE (a renamed or
/// reordered field, a wrong value) was entirely unverified.
pub fn decode_event<T>(logs: &[String]) -> Option<T>
where
    T: anchor_lang::Discriminator + anchor_lang::AnchorDeserialize,
{
    let disc = T::DISCRIMINATOR;
    for line in logs {
        let Some(b64) = line.strip_prefix("Program data: ") else {
            continue;
        };
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) else {
            continue;
        };
        if bytes.len() >= disc.len() && &bytes[..disc.len()] == disc {
            if let Ok(ev) = T::try_from_slice(&bytes[disc.len()..]) {
                return Some(ev);
            }
        }
    }
    None
}

// -------------------------------------------------------------------------
// cc_buyback wire format — v2
// -------------------------------------------------------------------------
//
// Declared here rather than imported from cc_buyback, on purpose: a rename or a
// reordering on CC's side must fail this suite rather than have the two repos
// quietly agree with each other. Shared by cc_buyback_v2.rs and pnft.rs so the
// declaration exists once — two hand-serialised copies of someone else's
// account layout is two places to get it wrong, with no compiler help.
//
// v2 changes from v1: no `client_program` and no `seller_nonce` in the digest;
// `payment_mint` added; `Policy` carries payment LANES instead of a single
// mint/treasury pair, and the client allow-list is gone.

use anchor_lang::solana_program::pubkey;
use litesvm::LiteSVM;
use solana_sdk::{
    account::Account,
    instruction::Instruction,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
};

pub const CC_BUYBACK_ID: Pubkey = pubkey!("CcBuyM7sDhedBGLZxivBvgZVdqzrQAG66KYHgnTTEpLF");
pub const CC_ED25519_ID: Pubkey = pubkey!("Ed25519SigVerify111111111111111111111111111");

/// `sha256("account:Policy")[..8]`.
pub const CC_POLICY_DISC: [u8; 8] = [222, 135, 7, 163, 235, 177, 33, 68];
pub const CC_QUOTE_DOMAIN: &[u8] = b"cc-buyback-quote-v2";
pub const CC_MAX_DESTINATIONS: usize = 32;
pub const CC_MAX_LANES: usize = 8;

/// One payment lane: a mint and the treasury token account that settles it.
#[derive(Clone, Copy)]
pub struct CcLane {
    pub mint: Pubkey,
    pub treasury: Pubkey,
}

pub fn cc_policy_pda() -> (Pubkey, u8) {
    Pubkey::find_program_address(&[b"policy"], &CC_BUYBACK_ID)
}

pub fn cc_rent_vault_pda() -> (Pubkey, u8) {
    Pubkey::find_program_address(&[b"rent"], &CC_BUYBACK_ID)
}

/// `[b"quote", digest]` — cc_buyback's single-use marker. Its existence after a
/// swap is the proof the quote was spent.
pub fn cc_quote_marker_pda(digest: &[u8; 32]) -> Pubkey {
    Pubkey::find_program_address(&[b"quote", digest], &CC_BUYBACK_ID).0
}

/// cc_buyback's `Policy`, serialized by hand.
pub fn plant_cc_policy(
    svm: &mut LiteSVM,
    policy: Pubkey,
    bump: u8,
    quote_signer: Pubkey,
    destinations: &[Pubkey],
    lanes: &[CcLane],
) {
    let mut d = Vec::new();
    d.extend_from_slice(&CC_POLICY_DISC);
    d.extend_from_slice(Pubkey::new_unique().as_ref()); // authority (Squads)
    d.extend_from_slice(Pubkey::default().as_ref()); // pending_authority
    d.extend_from_slice(quote_signer.as_ref());
    d.extend_from_slice(Pubkey::new_unique().as_ref()); // pauser
    d.push(0); // paused
    d.push(bump);
    d.push(destinations.len() as u8);
    d.push(lanes.len() as u8);
    for i in 0..CC_MAX_DESTINATIONS {
        d.extend_from_slice(destinations.get(i).copied().unwrap_or_default().as_ref());
    }
    for i in 0..CC_MAX_LANES {
        match lanes.get(i) {
            Some(l) => {
                d.extend_from_slice(l.mint.as_ref());
                d.extend_from_slice(l.treasury.as_ref());
            }
            None => d.extend_from_slice(&[0u8; 64]),
        }
    }
    d.extend_from_slice(&[0u8; 256]); // _padding

    svm.set_account(
        policy,
        Account {
            lamports: 10_000_000_000,
            data: d,
            owner: CC_BUYBACK_ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

/// The digest CC signs. Independently reimplemented from SPEC.md.
#[allow(clippy::too_many_arguments)]
pub fn cc_quote_digest(
    seller_authority: &Pubkey,
    asset: &Pubkey,
    destination_owner: &Pubkey,
    payment_mint: &Pubkey,
    price: u64,
    expires_at: i64,
    quote_id: u64,
    memo: &str,
) -> [u8; 32] {
    use anchor_lang::solana_program::hash::hashv;
    hashv(&[
        CC_QUOTE_DOMAIN,
        CC_BUYBACK_ID.as_ref(),
        seller_authority.as_ref(),
        asset.as_ref(),
        destination_owner.as_ref(),
        payment_mint.as_ref(),
        &price.to_le_bytes(),
        &expires_at.to_le_bytes(),
        &quote_id.to_le_bytes(),
        memo.as_bytes(),
    ])
    .to_bytes()
}

/// A top-level ed25519 precompile instruction over `message`. Must be top-level:
/// a precompile cannot be reached by CPI, and cc_buyback reads it out of the
/// instructions sysvar, which lists only top-level instructions.
pub fn cc_ed25519_ix(signer: &Keypair, message: &[u8]) -> Instruction {
    const HEADER: u16 = 16;
    let pk_off = HEADER;
    let sig_off = HEADER + 32;
    let msg_off = HEADER + 32 + 64;
    let sig = signer.sign_message(message);

    let mut data = Vec::with_capacity(msg_off as usize + message.len());
    data.push(1); // num_signatures
    data.push(0); // padding
    data.extend_from_slice(&sig_off.to_le_bytes());
    data.extend_from_slice(&u16::MAX.to_le_bytes());
    data.extend_from_slice(&pk_off.to_le_bytes());
    data.extend_from_slice(&u16::MAX.to_le_bytes());
    data.extend_from_slice(&msg_off.to_le_bytes());
    data.extend_from_slice(&(message.len() as u16).to_le_bytes());
    data.extend_from_slice(&u16::MAX.to_le_bytes());
    data.extend_from_slice(signer.pubkey().as_ref());
    data.extend_from_slice(sig.as_ref());
    data.extend_from_slice(message);

    Instruction { program_id: CC_ED25519_ID, accounts: vec![], data }
}
