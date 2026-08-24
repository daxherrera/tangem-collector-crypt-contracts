//! Spending-cap and input-guard tests for open_pack / withdraw_token on real
//! bytecode (LiteSVM), incl. the UTC-day rollover exercised by warping the
//! Clock sysvar — the only place the roll_day path runs (no other test
//! advances the calendar).
//!
//! Run `anchor build` first so target/deploy/tangem_gacha_vault.so exists.

mod common;

use anchor_lang::{AccountSerialize, InstructionData, ToAccountMetas};
use litesvm::LiteSVM;
use solana_sdk::{
    account::Account,
    clock::Clock,
    instruction::{AccountMeta, Instruction},
    pubkey,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction, system_program,
    transaction::Transaction,
};
use tangem_gacha_vault::{accounts, instruction, Config, CONFIG_SEED, VAULT_SEED};

const PROGRAM_ID: Pubkey = tangem_gacha_vault::ID;
const TOKEN_PROGRAM_ID: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const ATA_PROGRAM_ID: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
const SECONDS_PER_DAY: i64 = 86_400;
const D: u64 = 300_000_000; // per_spin_cap == daily_cap (fee_bps = 0)

fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), TOKEN_PROGRAM_ID.as_ref(), mint.as_ref()],
        &ATA_PROGRAM_ID,
    )
    .0
}

fn set_day(svm: &mut LiteSVM, unix: i64) {
    let mut clock = svm.get_sysvar::<Clock>();
    clock.unix_timestamp = unix;
    svm.set_sysvar::<Clock>(&clock);
}

fn day_start(day: i64) -> i64 {
    day * SECONDS_PER_DAY + 10
}

fn create_mint(svm: &mut LiteSVM, payer: &Keypair, authority: &Pubkey) -> Pubkey {
    let mint = Keypair::new();
    let rent = svm.minimum_balance_for_rent_exemption(82);
    let create = system_instruction::create_account(
        &payer.pubkey(),
        &mint.pubkey(),
        rent,
        82,
        &TOKEN_PROGRAM_ID,
    );
    let mut data = vec![20u8, 6]; // InitializeMint2, 6 decimals
    data.extend_from_slice(authority.as_ref());
    data.push(0);
    let init = Instruction {
        program_id: TOKEN_PROGRAM_ID,
        accounts: vec![AccountMeta::new(mint.pubkey(), false)],
        data,
    };
    let tx = Transaction::new_signed_with_payer(
        &[create, init],
        Some(&payer.pubkey()),
        &[payer, &mint],
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx).expect("create mint");
    mint.pubkey()
}

fn create_ata(svm: &mut LiteSVM, payer: &Keypair, owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    let addr = ata(owner, mint);
    let ix = Instruction {
        program_id: ATA_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new(addr, false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        ],
        data: vec![1], // CreateIdempotent
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[payer],
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx).expect("create ata");
    addr
}

fn mint_to(svm: &mut LiteSVM, payer: &Keypair, mint: &Pubkey, dest: &Pubkey, authority: &Keypair, amount: u64) {
    let mut data = vec![7u8];
    data.extend_from_slice(&amount.to_le_bytes());
    let ix = Instruction {
        program_id: TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*mint, false),
            AccountMeta::new(*dest, false),
            AccountMeta::new_readonly(authority.pubkey(), true),
        ],
        data,
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[payer, authority],
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx).expect("mint_to");
}

struct World {
    svm: LiteSVM,
    cold: Keypair,
    hot: Keypair,
    payer: Keypair,
    mint: Pubkey,
    vault: Pubkey,
    vault_usdc: Pubkey,
    config: Pubkey,
    gacha_usdc: Pubkey,
    fee_usdc: Pubkey,
}

fn setup() -> World {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(
        PROGRAM_ID,
        common::program_so_path(),
    )
    .expect("run `anchor build` first");

    let payer = Keypair::new();
    let cold = Keypair::new();
    let hot = Keypair::new();
    for kp in [&payer, &cold, &hot] {
        svm.airdrop(&kp.pubkey(), 10_000_000_000).unwrap();
    }
    set_day(&mut svm, day_start(100));

    let mint = create_mint(&mut svm, &payer, &payer.pubkey());
    let (vault, _) = Pubkey::find_program_address(&[VAULT_SEED, cold.pubkey().as_ref()], &PROGRAM_ID);
    let vault_usdc = create_ata(&mut svm, &payer, &vault, &mint);
    mint_to(&mut svm, &payer, &mint, &vault_usdc, &payer, 100 * D);
    let gacha_owner = Keypair::new();
    let gacha_usdc = create_ata(&mut svm, &payer, &gacha_owner.pubkey(), &mint);
    let fee_owner = Keypair::new();
    let fee_usdc = create_ata(&mut svm, &payer, &fee_owner.pubkey(), &mint);

    // Config written directly (initialize_config's upgrade-authority gate needs
    // ProgramData, which litesvm's add_program does not create); fee_bps = 0.
    let (config, bump) = Pubkey::find_program_address(&[CONFIG_SEED], &PROGRAM_ID);
    let cfg = Config {
        admin: Pubkey::new_unique(),
        usdc_mint: mint,
        gacha_wallet: gacha_owner.pubkey(),
        gacha_usdc_account: gacha_usdc,
        fee_usdc_account: fee_usdc,
        fee_bps: 0,
        paused: false,
        _reserved: false,
        allow_buyback_delegation: false,
        bump,
    };
    let mut data = Vec::new();
    cfg.try_serialize(&mut data).unwrap();
    let lamports = svm.minimum_balance_for_rent_exemption(data.len());
    svm.set_account(
        config,
        Account { lamports, data, owner: PROGRAM_ID, executable: false, rent_epoch: 0 },
    )
    .unwrap();

    let ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::InitVault {
            vault,
            cold_owner: cold.pubkey(),
            payer: payer.pubkey(),
            system_program: system_program::ID,
        }
        .to_account_metas(None),
        data: instruction::InitVault { hot_delegate: hot.pubkey(), per_spin_cap: D, daily_cap: D }.data(),
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[&payer, &cold],
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx).expect("init_vault");

    World { svm, cold, hot, payer, mint, vault, vault_usdc, config, gacha_usdc, fee_usdc }
}

/// Rewrites the config PDA in place — the direct-write analogue of an admin
/// `update_config`, for pointing treasuries at arbitrary accounts.
fn set_config(w: &mut World, gacha_usdc_account: Pubkey, fee_usdc_account: Pubkey) {
    let (config, bump) = Pubkey::find_program_address(&[CONFIG_SEED], &PROGRAM_ID);
    let cfg = Config {
        admin: Pubkey::new_unique(),
        usdc_mint: w.mint,
        gacha_wallet: Pubkey::new_unique(),
        gacha_usdc_account,
        fee_usdc_account,
        fee_bps: 0,
        paused: false,
        _reserved: false,
        allow_buyback_delegation: false,
        bump,
    };
    let mut data = Vec::new();
    cfg.try_serialize(&mut data).unwrap();
    let lamports = w.svm.minimum_balance_for_rent_exemption(data.len());
    w.svm
        .set_account(
            config,
            Account { lamports, data, owner: PROGRAM_ID, executable: false, rent_epoch: 0 },
        )
        .unwrap();
}

fn send_spin(w: &mut World, amount: u64, tag: u8) -> Result<(), String> {
    // unique memo per call so identical spins do not collide on the
    // blockhash-based dedup
    send_spin_memo(w, amount, format!("dev-test-{tag}:open"))
}

fn send_spin_memo(w: &mut World, amount: u64, memo: String) -> Result<(), String> {
    let ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::OpenPack {
            config: w.config,
            vault: w.vault,
            hot_delegate: w.hot.pubkey(),
            usdc_mint: w.mint,
            vault_usdc: w.vault_usdc,
            gacha_usdc: w.gacha_usdc,
            fee_usdc: w.fee_usdc,
            token_program: TOKEN_PROGRAM_ID,
        }
        .to_account_metas(None),
        data: instruction::OpenPack { amount, memo }.data(),
    };
    w.svm.expire_blockhash();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&w.hot.pubkey()),
        &[&w.hot],
        w.svm.latest_blockhash(),
    );
    w.svm
        .send_transaction(tx)
        .map(|_| ())
        .map_err(|f| f.meta.logs.join("\n"))
}

/// Two full-cap spins on the same UTC day: the second must be refused.
#[test]
fn same_day_second_full_spin_blocked() {
    let mut w = setup();
    send_spin(&mut w, D, 1).expect("first spin fits the fresh day");
    let err = send_spin(&mut w, D, 2).expect_err("second same-day spin must exceed the cap");
    assert!(err.contains("ExceedsDailyCap"), "got:\n{err}");
}

/// After UTC midnight the budget resets and a full-cap spin fits again.
#[test]
fn day_rollover_resets_the_budget() {
    let mut w = setup();
    send_spin(&mut w, D, 1).expect("day-0 spin");
    let err = send_spin(&mut w, D, 2).expect_err("same-day repeat blocked");
    assert!(err.contains("ExceedsDailyCap"), "got:\n{err}");

    set_day(&mut w.svm, day_start(101));
    send_spin(&mut w, D, 3).expect("fresh UTC day allows a full spin again");
}

/// per_spin_cap binds independently of the daily budget.
#[test]
fn per_spin_cap_enforced() {
    let mut w = setup();
    let err = send_spin(&mut w, D + 1, 1).expect_err("above per-spin cap");
    assert!(err.contains("ExceedsPerSpinCap"), "got:\n{err}");
}

/// The memo is CC's reconciliation key: an empty or over-long one is refused
/// before any USDC moves.
#[test]
fn memo_bounds_enforced() {
    let mut w = setup();
    let err = send_spin_memo(&mut w, D, String::new()).expect_err("empty memo must fail");
    assert!(err.contains("InvalidMemo"), "got:\n{err}");

    let err = send_spin_memo(&mut w, D, "x".repeat(257)).expect_err("over-long memo must fail");
    assert!(err.contains("InvalidMemo"), "got:\n{err}");

    // 256 is the boundary and must still pass.
    send_spin_memo(&mut w, D, "x".repeat(256)).expect("256-byte memo is the limit, not past it");
}

/// A zero-amount spin is refused (it would burn a memo for nothing).
#[test]
fn zero_amount_spin_rejected() {
    let mut w = setup();
    let err = send_spin(&mut w, 0, 1).expect_err("zero amount must fail");
    assert!(err.contains("ZeroAmount"), "got:\n{err}");
}

/// A wrong-mint treasury in the config must fail the FIRST spin loudly — even
/// at fee_bps = 0, when no fee transfer would touch the account. Without the
/// `token::mint` pin this misconfiguration passes silently until the fee is
/// raised, then breaks every spin of every vault at once.
#[test]
fn wrong_mint_fee_treasury_fails_first_spin_even_at_zero_fee() {
    let mut w = setup();

    let payer = w.payer.insecure_clone();
    let wrong_mint = create_mint(&mut w.svm, &payer, &payer.pubkey());
    let wrong_owner = Keypair::new();
    let wrong_fee = create_ata(&mut w.svm, &payer, &wrong_owner.pubkey(), &wrong_mint);

    let (gacha_usdc, real_fee) = (w.gacha_usdc, w.fee_usdc);
    set_config(&mut w, gacha_usdc, wrong_fee);
    w.fee_usdc = wrong_fee; // pass the account the config now names
    let err = send_spin(&mut w, D, 1).expect_err("wrong-mint fee treasury must fail the spin");
    assert!(err.contains("ConstraintTokenMint"), "got:\n{err}");

    // Positive control: restoring the correct account makes the same spin pass,
    // so the failure above is the mint pin and nothing else.
    set_config(&mut w, gacha_usdc, real_fee);
    w.fee_usdc = real_fee;
    send_spin(&mut w, D, 2).expect("correct fee treasury spins fine");
}

/// Same trap on the spin-payment leg: a wrong-mint `gacha_usdc_account` must
/// be a constraint failure, not a token-program error inside the CPI.
#[test]
fn wrong_mint_gacha_treasury_fails_spin() {
    let mut w = setup();

    let payer = w.payer.insecure_clone();
    let wrong_mint = create_mint(&mut w.svm, &payer, &payer.pubkey());
    let wrong_owner = Keypair::new();
    let wrong_gacha = create_ata(&mut w.svm, &payer, &wrong_owner.pubkey(), &wrong_mint);

    let fee_usdc = w.fee_usdc;
    set_config(&mut w, wrong_gacha, fee_usdc);
    w.gacha_usdc = wrong_gacha; // pass the account the config now names
    let err = send_spin(&mut w, D, 1).expect_err("wrong-mint gacha treasury must fail the spin");
    assert!(err.contains("ConstraintTokenMint"), "got:\n{err}");
}

fn send_withdraw(w: &mut World, destination: Pubkey, amount: u64) -> Result<(), String> {
    let ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::WithdrawToken {
            vault: w.vault,
            cold_owner: w.cold.pubkey(),
            source: w.vault_usdc,
            mint: w.mint,
            destination,
            token_program: TOKEN_PROGRAM_ID,
        }
        .to_account_metas(None),
        data: instruction::WithdrawToken { amount }.data(),
    };
    w.svm.expire_blockhash();
    let cold = w.cold.insecure_clone();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&cold.pubkey()),
        &[&cold],
        w.svm.latest_blockhash(),
    );
    w.svm
        .send_transaction(tx)
        .map(|_| ())
        .map_err(|f| f.meta.logs.join("\n"))
}

/// `withdraw_token` with the source as its own destination must be rejected:
/// spl-token's self-transfer returns Ok having moved nothing, and the
/// TokenWithdrawn event would falsely report the full amount.
#[test]
fn withdraw_token_rejects_source_as_destination() {
    let mut w = setup();
    let vault_usdc = w.vault_usdc;
    let err = send_withdraw(&mut w, vault_usdc, D).expect_err("self-transfer must fail");
    assert!(err.contains("Unauthorized"), "got:\n{err}");

    // Positive control: a real destination of the same mint receives the funds.
    let payer = w.payer.insecure_clone();
    let recipient = Keypair::new();
    let dest = create_ata(&mut w.svm, &payer, &recipient.pubkey(), &w.mint);
    send_withdraw(&mut w, dest, D).expect("withdraw to a distinct account succeeds");
    let acc = w.svm.get_account(&dest).unwrap();
    let amount = u64::from_le_bytes(acc.data[64..72].try_into().unwrap());
    assert_eq!(amount, D, "the full amount must actually arrive");
}
