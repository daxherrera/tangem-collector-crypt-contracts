//! The vault calling Collector Crypt's buyback program.
//!
//! This is the test that proves the architecture: CC signs a quote offline and
//! is not a signer on the transaction at all, the vault moves the asset, and
//! only then CPIs into cc_buyback for the money. It runs entirely in-process —
//! litesvm loads both programs plus the real dumped mpl-core, so `cargo test`
//! exercises the whole integration with no validator and no network.
//!
//! ORDERING (v2): asset first, money second. cc_buyback reads the
//! processed-sibling list and refuses to pay unless the transfer has already
//! completed, so the v1 "money first" ordering now fails outright. Replay is
//! cc_buyback's job too — a `[b"quote", digest]` marker — so the vault keeps no
//! counter and `Vault.buyback_nonce` is vestigial.
//!
//! Everything about cc_buyback is declared INDEPENDENTLY, in common/mod.rs: the
//! Policy layout, the discriminators, the digest preimage. A rename or a
//! reordering on CC's side must fail this suite rather than have the two repos
//! quietly agree with each other.

mod common;

use anchor_lang::{InstructionData, ToAccountMetas};
use litesvm::LiteSVM;
use solana_sdk::{
    account::Account,
    instruction::Instruction,
    pubkey,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_program,
    sysvar,
    transaction::Transaction,
};
use tangem_gacha_vault::{accounts, instruction, CC_BUYBACK_ID, CONFIG_SEED, VAULT_SEED};

use common::{
    cc_ed25519_ix as ed25519_ix, cc_quote_digest as quote_digest, cc_quote_marker_pda,
    cc_rent_vault_pda, plant_cc_policy, CcLane,
};


const PROGRAM_ID: Pubkey = pubkey!("29agFEruMu2jedVwnDgKPuq7ejmTiB7sEqzdhufQQS9u");
const MPL_CORE_ID: Pubkey = pubkey!("CoREENxT6tW1HoK8ypY1SxRMZTcVPm7R94rH4PZNhX7d");
const CC_CORE_COLLECTION: Pubkey = pubkey!("CCryptUfeFSZ3Fgc9FLeKrhLVAP67FSqi1GuVoj9CRac");
const CC_CORE_ASSET: Pubkey = pubkey!("13fCVtpxtzN8mv8jERe6Ev7rSuXM4nbSwFGhmauvKB7b");
const TOKEN_PROGRAM_ID: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const ATA_PROGRAM_ID: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

const MEMO: &str = "dev-00000000-1111-2222-3333-444444444444:buyback";
const PRICE: u64 = 10_000_000;

fn fixture(name: &str) -> String {
    format!("{}/../../tests/fixtures/{}", env!("CARGO_MANIFEST_DIR"), name)
}

fn load_account_dump(svm: &mut LiteSVM, path: &str, address: Pubkey, patch_owner: Option<Pubkey>) {
    let raw = std::fs::read_to_string(path).expect("account dump");
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let acct = &v["account"];
    let b64 = acct["data"][0].as_str().unwrap();
    use base64::Engine;
    let mut data = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
    if let Some(owner) = patch_owner {
        data[1..33].copy_from_slice(owner.as_ref());
    }
    let owner_prog: Pubkey = acct["owner"].as_str().unwrap().parse().unwrap();
    svm.set_account(
        address,
        Account {
            lamports: acct["lamports"].as_u64().unwrap(),
            data,
            owner: owner_prog,
            executable: acct["executable"].as_bool().unwrap(),
            rent_epoch: 0,
        },
    )
    .unwrap();
}

struct World {
    svm: LiteSVM,
    payer: Keypair,
    cold: Keypair,
    hot: Keypair,
    vault: Pubkey,
    config: Pubkey,
    mint: Pubkey,
    cc_treasury: Pubkey,
    vault_usdc: Pubkey,
    cc_policy: Pubkey,
    quote_signer: Keypair,
    destination: Pubkey,
}

fn setup() -> World {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(PROGRAM_ID, common::program_so_path()).expect("run cargo-build-sbf");
    svm.add_program_from_file(MPL_CORE_ID, fixture("mpl_core.so")).expect("mpl_core.so");
    svm.add_program_from_file(CC_BUYBACK_ID, fixture("cc_buyback.so")).expect("cc_buyback.so");

    let payer = Keypair::new();
    let cold = Keypair::new();
    let hot = Keypair::new();
    let quote_signer = Keypair::new();
    let cc_wallet = Keypair::new();
    for kp in [&payer, &cold, &hot, &cc_wallet] {
        svm.airdrop(&kp.pubkey(), 10_000_000_000).unwrap();
    }

    let (vault, _) = Pubkey::find_program_address(&[VAULT_SEED, cold.pubkey().as_ref()], &PROGRAM_ID);
    let (config, config_bump) = Pubkey::find_program_address(&[CONFIG_SEED], &PROGRAM_ID);
    let (cc_policy, cc_bump) = Pubkey::find_program_address(&[b"policy"], &CC_BUYBACK_ID);
    let destination = Pubkey::new_unique();

    load_account_dump(&mut svm, &fixture("cc_core_collection.json"), CC_CORE_COLLECTION, None);
    load_account_dump(&mut svm, &fixture("cc_core_asset.json"), CC_CORE_ASSET, Some(vault));

    // USDC world.
    let mint = Pubkey::new_unique();
    plant_mint(&mut svm, mint);
    let cc_treasury = Pubkey::new_unique();
    // Delegated to cc_buyback's policy PDA, owner still CC's wallet.
    plant_token(&mut svm, cc_treasury, mint, cc_wallet.pubkey(), 1_000_000_000, Some((cc_policy, 500_000_000)));
    let vault_usdc = ata(&mint, &vault);
    plant_token(&mut svm, vault_usdc, mint, vault, 0, None);

    plant_config(&mut svm, config, config_bump, mint, cc_wallet.pubkey());
    plant_cc_policy(
        &mut svm,
        cc_policy,
        cc_bump,
        quote_signer.pubkey(),
        &[destination],
        &[CcLane { mint, treasury: cc_treasury }],
    );
    // cc_buyback fronts the quote marker's rent from its own PDA, so the phone
    // never pays for CC's bookkeeping. Unfunded, every buyback fails.
    svm.airdrop(&cc_rent_vault_pda().0, 1_000_000_000).unwrap();

    // Real init_vault so the nonce starts where the program puts it.
    let ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::InitVault { vault, cold_owner: cold.pubkey(), payer: payer.pubkey(), system_program: system_program::ID }.to_account_metas(None),
        data: instruction::InitVault { hot_delegate: hot.pubkey(), per_spin_cap: 300_000_000, daily_cap: 1_000_000_000 }.data(),
    };
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer, &cold], svm.latest_blockhash());
    svm.send_transaction(tx).unwrap();

    World { svm, payer, cold, hot, vault, config, mint, cc_treasury, vault_usdc, cc_policy, quote_signer, destination }
}

fn ata(mint: &Pubkey, owner: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[owner.as_ref(), TOKEN_PROGRAM_ID.as_ref(), mint.as_ref()], &ATA_PROGRAM_ID).0
}

fn plant_mint(svm: &mut LiteSVM, mint: Pubkey) {
    // SPL Mint, 82 bytes: [0..4] authority COption tag, [4..36] authority,
    // [36..44] supply, [44] decimals, [45] is_initialized, [46..50] freeze tag,
    // [50..82] freeze authority.
    let mut data = vec![0u8; 82];
    data[36..44].copy_from_slice(&1_000_000_000_000u64.to_le_bytes());
    data[44] = 6; // decimals
    data[45] = 1; // is_initialized
    svm.set_account(mint, Account { lamports: 1_000_000_000, data, owner: TOKEN_PROGRAM_ID, executable: false, rent_epoch: 0 }).unwrap();
}

fn plant_token(svm: &mut LiteSVM, key: Pubkey, mint: Pubkey, owner: Pubkey, amount: u64, delegate: Option<(Pubkey, u64)>) {
    let mut data = vec![0u8; 165];
    data[0..32].copy_from_slice(mint.as_ref());
    data[32..64].copy_from_slice(owner.as_ref());
    data[64..72].copy_from_slice(&amount.to_le_bytes());
    if let Some((d, amt)) = delegate {
        data[72..76].copy_from_slice(&1u32.to_le_bytes()); // COption::Some
        data[76..108].copy_from_slice(d.as_ref());
        data[121..129].copy_from_slice(&amt.to_le_bytes()); // delegated_amount
    }
    data[108] = 1; // AccountState::Initialized
    svm.set_account(key, Account { lamports: 1_000_000_000, data, owner: TOKEN_PROGRAM_ID, executable: false, rent_epoch: 0 }).unwrap();
}

fn plant_config(svm: &mut LiteSVM, config: Pubkey, bump: u8, mint: Pubkey, rent_destination: Pubkey) {
    use anchor_lang::AccountSerialize;
    let cfg = tangem_gacha_vault::Config {
        admin: Pubkey::new_unique(),
        pending_admin: Pubkey::default(),
        usdc_mint: mint,
        rent_destination,
        gacha_usdc_account: Pubkey::new_unique(),
        fee_usdc_account: Pubkey::new_unique(),
        fee_bps: 0,
        paused: false,
        _reserved: false,
        allow_buyback_delegation: false,
        bump,
        _padding: [0u8; 128],
    };
    let mut data = Vec::new();
    cfg.try_serialize(&mut data).unwrap();
    svm.set_account(config, Account { lamports: 10_000_000_000, data, owner: PROGRAM_ID, executable: false, rent_epoch: 0 }).unwrap();
}

fn token_amount(w: &World, key: Pubkey) -> u64 {
    let a = w.svm.get_account(&key).unwrap();
    u64::from_le_bytes(a.data[64..72].try_into().unwrap())
}

fn asset_owner(w: &World) -> Pubkey {
    let a = w.svm.get_account(&CC_CORE_ASSET).unwrap();
    Pubkey::try_from(&a.data[1..33]).unwrap()
}

/// `Vault.buyback_nonce` survives only so the account layout does not shift —
/// the branch is a layout freeze. v2 never reads or increments it; replay is
/// cc_buyback's `[b"quote", digest]` marker. Asserted to stay 0 so a
/// reintroduced counter is noticed.
fn vault_nonce(w: &World) -> u64 {
    use anchor_lang::AccountDeserialize;
    let a = w.svm.get_account(&w.vault).unwrap();
    tangem_gacha_vault::Vault::try_deserialize(&mut a.data.as_slice()).unwrap().buyback_nonce
}

fn quote_marker_exists(w: &World, digest: &[u8; 32]) -> bool {
    w.svm.get_account(&cc_quote_marker_pda(digest)).is_some_and(|a| !a.data.is_empty())
}

fn buyback_ix(w: &World, price: u64, quote_id: u64, expires_at: i64) -> Instruction {
    // The marker PDA is keyed on the digest, so the caller has to recompute it
    // — exactly what a real integrator does.
    let digest = quote_digest(&w.vault, &CC_CORE_ASSET, &w.destination, &w.mint, price, expires_at, quote_id, MEMO);
    Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::BuybackCoreV2 {
            cc_quote_marker: cc_quote_marker_pda(&digest),
            cc_rent_vault: cc_rent_vault_pda().0,
            config: w.config,
            vault: w.vault,
            hot_delegate: w.hot.pubkey(),
            destination_owner: w.destination,
            usdc_mint: w.mint,
            cc_usdc: w.cc_treasury,
            vault_usdc: w.vault_usdc,
            asset: CC_CORE_ASSET,
            collection: CC_CORE_COLLECTION,
            mpl_core_program: MPL_CORE_ID,
            cc_program: CC_BUYBACK_ID,
            cc_policy: w.cc_policy,
            sysvar_instructions: sysvar::instructions::ID,
            token_program: TOKEN_PROGRAM_ID,
            system_program: system_program::ID,
        }
        .to_account_metas(None),
        data: instruction::BuybackCoreV2 { price, quote_id, expires_at, memo: MEMO.to_string() }.data(),
    }
}

fn send(w: &mut World, price: u64, quote_id: u64) -> Result<(), litesvm::types::FailedTransactionMetadata> {
    let digest = quote_digest(&w.vault, &CC_CORE_ASSET, &w.destination, &w.mint, price, i64::MAX, quote_id, MEMO);
    let quote_ix = ed25519_ix(&w.quote_signer, &digest);
    let ix = buyback_ix(w, price, quote_id, i64::MAX);
    let payer = w.payer.insecure_clone();
    let hot = w.hot.insecure_clone();
    let tx = Transaction::new_signed_with_payer(&[quote_ix, ix], Some(&hot.pubkey()), &[&hot], w.svm.latest_blockhash());
    let _ = payer;
    w.svm.send_transaction(tx).map(|_| ())
}

// ---------------------------------------------------------------------------

/// The whole architecture in one test: CC signs a quote and is not a signer on
/// the transaction; the vault moves the Core asset and then CPIs cc_buyback for
/// the money; the phone pays the fee.
#[test]
fn vault_cpis_cc_buyback_and_swaps_the_core_asset() {
    let mut w = setup();
    assert_eq!(asset_owner(&w), w.vault);
    assert_eq!(vault_nonce(&w), 0);
    let treasury_before = token_amount(&w, w.cc_treasury);
    let digest = quote_digest(&w.vault, &CC_CORE_ASSET, &w.destination, &w.mint, PRICE, i64::MAX, 1, MEMO);
    assert!(!quote_marker_exists(&w, &digest));

    send(&mut w, PRICE, 1).expect("atomic buyback should land");

    assert_eq!(token_amount(&w, w.vault_usdc), PRICE, "vault was not paid");
    assert_eq!(token_amount(&w, w.cc_treasury), treasury_before - PRICE);
    assert_eq!(asset_owner(&w), w.destination, "asset did not reach the prize wallet");

    // Replay protection moved to CC: a marker account, not a counter here.
    assert!(quote_marker_exists(&w, &digest), "cc_buyback did not mark the quote spent");
    assert_eq!(vault_nonce(&w), 0, "v2 must not touch buyback_nonce — it is layout padding now");
}

/// The replay the marker exists for: the card comes back to the vault inside the
/// quote's TTL — which CC's own webhook makes routine, since it re-pools a card
/// the moment a buyback confirms — and the original quote is presented again.
/// In v1 the vault's own counter caught this; now cc_buyback's marker does, so
/// no integrating program has to keep state.
#[test]
fn a_returned_asset_cannot_be_bought_twice_with_one_quote() {
    let mut w = setup();
    send(&mut w, PRICE, 1).unwrap();
    let paid_once = token_amount(&w, w.vault_usdc);

    // The prize is delivered back to the same vault.
    load_account_dump(&mut w.svm, &fixture("cc_core_asset.json"), CC_CORE_ASSET, Some(w.vault));
    assert_eq!(asset_owner(&w), w.vault);

    let res = send(&mut w, PRICE, 1);
    assert!(res.is_err(), "the same quote was spent twice");
    assert_eq!(token_amount(&w, w.vault_usdc), paid_once, "CC paid twice for one card");
    assert_eq!(asset_owner(&w), w.vault, "asset moved on a rejected replay");
}

#[test]
fn rejects_a_quote_signed_by_the_wrong_key() {
    let mut w = setup();
    let impostor = Keypair::new();
    let digest = quote_digest(&w.vault, &CC_CORE_ASSET, &w.destination, &w.mint, PRICE, i64::MAX, 1, MEMO);
    let quote_ix = ed25519_ix(&impostor, &digest);
    let ix = buyback_ix(&w, PRICE, 1, i64::MAX);
    let hot = w.hot.insecure_clone();
    let tx = Transaction::new_signed_with_payer(&[quote_ix, ix], Some(&hot.pubkey()), &[&hot], w.svm.latest_blockhash());

    assert!(w.svm.send_transaction(tx).is_err());
    assert_eq!(asset_owner(&w), w.vault);
    assert_eq!(token_amount(&w, w.vault_usdc), 0);
}

/// Price is bound to the signature, not merely `> 0` as in v1. This is the
/// concrete thing the quote model buys: a $10,000 card can no longer leave for
/// one micro-USDC.
#[test]
fn rejects_a_price_the_quote_did_not_authorize() {
    let mut w = setup();
    let digest = quote_digest(&w.vault, &CC_CORE_ASSET, &w.destination, &w.mint, PRICE, i64::MAX, 1, MEMO);
    let quote_ix = ed25519_ix(&w.quote_signer, &digest);
    let ix = buyback_ix(&w, 1, 1, i64::MAX); // 1 micro-USDC
    let hot = w.hot.insecure_clone();
    let tx = Transaction::new_signed_with_payer(&[quote_ix, ix], Some(&hot.pubkey()), &[&hot], w.svm.latest_blockhash());

    assert!(w.svm.send_transaction(tx).is_err(), "an unquoted price was accepted");
    assert_eq!(asset_owner(&w), w.vault);
}

#[test]
fn rejects_a_destination_cc_has_not_allow_listed() {
    let mut w = setup();
    w.destination = Pubkey::new_unique();
    assert!(send(&mut w, PRICE, 1).is_err());
    assert_eq!(asset_owner(&w), w.vault);
}

#[test]
fn rejects_a_transaction_with_no_quote() {
    let mut w = setup();
    let ix = buyback_ix(&w, PRICE, 1, i64::MAX);
    let hot = w.hot.insecure_clone();
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&hot.pubkey()), &[&hot], w.svm.latest_blockhash());
    assert!(w.svm.send_transaction(tx).is_err());
    assert_eq!(asset_owner(&w), w.vault);
}

#[test]
fn rejects_a_foreign_cc_program() {
    let mut w = setup();
    let digest = quote_digest(&w.vault, &CC_CORE_ASSET, &w.destination, &w.mint, PRICE, i64::MAX, 1, MEMO);
    let quote_ix = ed25519_ix(&w.quote_signer, &digest);

    let mut ix = buyback_ix(&w, PRICE, 1, i64::MAX);
    for m in ix.accounts.iter_mut() {
        if m.pubkey == CC_BUYBACK_ID {
            m.pubkey = Pubkey::new_unique();
        }
    }
    let hot = w.hot.insecure_clone();
    let tx = Transaction::new_signed_with_payer(&[quote_ix, ix], Some(&hot.pubkey()), &[&hot], w.svm.latest_blockhash());
    assert!(w.svm.send_transaction(tx).is_err());
}

/// CC is not a signer. Worth asserting explicitly, because it is the whole
/// difference from v1 — and something a future refactor could quietly undo.
#[test]
fn collector_crypt_signs_nothing_on_chain() {
    let mut w = setup();
    let digest = quote_digest(&w.vault, &CC_CORE_ASSET, &w.destination, &w.mint, PRICE, i64::MAX, 1, MEMO);
    let quote_ix = ed25519_ix(&w.quote_signer, &digest);
    let ix = buyback_ix(&w, PRICE, 1, i64::MAX);

    assert!(ix.accounts.iter().all(|m| !m.is_signer || m.pubkey == w.hot.pubkey()),
        "someone other than the phone is a required signer");

    let hot = w.hot.insecure_clone();
    let tx = Transaction::new_signed_with_payer(&[quote_ix, ix], Some(&hot.pubkey()), &[&hot], w.svm.latest_blockhash());
    assert_eq!(tx.signatures.len(), 1, "more than one signature required");
    w.svm.send_transaction(tx).expect("single-signature buyback should land");
}

/// The whole transaction must fit one packet without a lookup table.
#[test]
fn core_v2_fits_a_legacy_packet() {
    let mut w = setup();
    let digest = quote_digest(&w.vault, &CC_CORE_ASSET, &w.destination, &w.mint, PRICE, i64::MAX, 1, MEMO);
    let quote_ix = ed25519_ix(&w.quote_signer, &digest);
    let memo_ix = Instruction {
        program_id: pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr"),
        accounts: vec![],
        data: MEMO.as_bytes().to_vec(),
    };
    let ix = buyback_ix(&w, PRICE, 1, i64::MAX);
    let hot = w.hot.insecure_clone();
    let tx = Transaction::new_signed_with_payer(&[quote_ix, ix, memo_ix], Some(&hot.pubkey()), &[&hot], w.svm.latest_blockhash());

    // Same accounting pnft.rs uses: shortvec(sig count) + 64 per signature +
    // the message. One signature here, because CC no longer signs.
    let message_size = tx.message_data().len();
    let tx_size = 1 + 64 + message_size;
    println!("buyback_core_v2 (ed25519 quote + 15 accounts + top-level memo), 1 signature:");
    println!("  message: {message_size} bytes");
    println!("  serialized transaction: {tx_size} bytes (Solana cap 1232)");
    assert!(tx_size <= 1232, "buyback_core_v2 must fit one packet ({tx_size} > 1232)");
}
