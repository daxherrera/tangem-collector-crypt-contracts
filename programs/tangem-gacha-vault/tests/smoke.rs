//! In-process execution smoke test using the LiteSVM Rust crate.
//!
//! This runs the REAL compiled program bytecode (target/deploy/*.so) inside an
//! in-process SVM — no solana-test-validator needed, so it is fast and works in
//! any environment. It proves the deployed artifact actually executes and that
//! the core authorization / cap logic behaves. (The full TS suite is
//! `anchor test`.)

mod common;

use anchor_lang::{AccountDeserialize, InstructionData, ToAccountMetas};
use litesvm::LiteSVM;
use solana_sdk::{
    instruction::Instruction,
    message::Message,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_program,
    transaction::Transaction,
};
use tangem_gacha_vault::{accounts, instruction, Vault, VAULT_SEED};

const PROGRAM_ID: Pubkey = tangem_gacha_vault::ID;

fn load() -> LiteSVM {
    let mut svm = LiteSVM::new();
    let so = common::program_so_path();
    svm.add_program_from_file(PROGRAM_ID, so)
        .expect("load program .so — run `cargo build-sbf` first");
    svm
}

fn vault_pda(cold: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[VAULT_SEED, cold.as_ref()], &PROGRAM_ID)
}

fn init_vault_ix(vault: Pubkey, cold: Pubkey, payer: Pubkey, hot: Pubkey, per: u64, daily: u64) -> Instruction {
    Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::InitVault {
            vault,
            cold_owner: cold,
            payer,
            system_program: system_program::ID,
        }
        .to_account_metas(None),
        data: instruction::InitVault {
            hot_delegate: hot,
            per_spin_cap: per,
            daily_cap: daily,
        }
        .data(),
    }
}

#[test]
fn init_vault_executes_and_persists_state() {
    let mut svm = load();
    let cold = Keypair::new();
    let payer = Keypair::new();
    let hot = Keypair::new();
    svm.airdrop(&payer.pubkey(), 5_000_000_000).unwrap();

    let (vault, _) = vault_pda(&cold.pubkey());
    let ix = init_vault_ix(vault, cold.pubkey(), payer.pubkey(), hot.pubkey(), 300_000_000, 1_000_000_000);
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[&payer, &cold],
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx).expect("init_vault should succeed");

    // Real bytecode ran: the vault account exists, is owned by the program, and
    // deserializes with exactly the state init_vault wrote.
    let acct = svm.get_account(&vault).expect("vault account created");
    assert_eq!(acct.owner, PROGRAM_ID);
    let v = Vault::try_deserialize(&mut acct.data.as_slice()).expect("deserialize Vault");
    assert_eq!(v.cold_owner, cold.pubkey());
    assert_eq!(v.hot_delegate, hot.pubkey());
    assert_eq!(v.per_spin_cap, 300_000_000);
    assert_eq!(v.daily_cap, 1_000_000_000);
    assert_eq!(v.spent_today, 0);
    assert!(v.live_buyback_mint.is_none());
    assert!(v.live_buyback_token.is_none());
}

#[test]
fn init_vault_rejects_invalid_caps() {
    let mut svm = load();
    let cold = Keypair::new();
    let payer = Keypair::new();
    let hot = Keypair::new();
    svm.airdrop(&payer.pubkey(), 5_000_000_000).unwrap();

    let (vault, _) = vault_pda(&cold.pubkey());
    // daily_cap < per_spin_cap must be rejected by the InvalidCaps require!.
    let ix = init_vault_ix(vault, cold.pubkey(), payer.pubkey(), hot.pubkey(), 1_000_000_000, 10_000_000);
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[&payer, &cold],
        svm.latest_blockhash(),
    );
    assert!(
        svm.send_transaction(tx).is_err(),
        "init_vault must reject daily_cap < per_spin_cap"
    );
    assert!(svm.get_account(&vault).map(|a| a.data.is_empty()).unwrap_or(true));
}

#[test]
fn init_vault_requires_cold_signature() {
    let mut svm = load();
    let cold = Keypair::new();
    let payer = Keypair::new();
    let hot = Keypair::new();
    svm.airdrop(&payer.pubkey(), 5_000_000_000).unwrap();

    let (vault, _) = vault_pda(&cold.pubkey());
    let ix = init_vault_ix(vault, cold.pubkey(), payer.pubkey(), hot.pubkey(), 300_000_000, 1_000_000_000);
    // Sign with payer only — cold_owner is a required Signer, so the SVM must
    // reject it. (`new_signed_with_payer` would panic client-side with
    // NotEnoughSigners before reaching the SVM, hence partial_sign.)
    let msg = Message::new(&[ix], Some(&payer.pubkey()));
    let mut tx = Transaction::new_unsigned(msg);
    tx.partial_sign(&[&payer], svm.latest_blockhash());
    assert!(
        svm.send_transaction(tx).is_err(),
        "init_vault must require the cold_owner signature"
    );
}
