//! Permissionless rent-sweep tests on real bytecode (LiteSVM): a random
//! caller closes an EMPTY vault-owned prize ATA and the rent lands on the
//! config-pinned CC gacha wallet; everything else is refused.
//!
//! Run `anchor build` first so target/deploy/tangem_gacha_vault.so exists.

mod common;

use anchor_lang::{AccountSerialize, InstructionData, ToAccountMetas};
use litesvm::LiteSVM;
use solana_sdk::{
    account::Account,
    instruction::{AccountMeta, Instruction},
    pubkey,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction, system_program,
    transaction::Transaction,
};
use tangem_gacha_vault::{
    accounts, instruction, Config, PrizeAtaSwept, Vault, CONFIG_SEED, VAULT_SEED,
};

const PROGRAM_ID: Pubkey = tangem_gacha_vault::ID;
const TOKEN_PROGRAM_ID: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const ATA_PROGRAM_ID: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), TOKEN_PROGRAM_ID.as_ref(), mint.as_ref()],
        &ATA_PROGRAM_ID,
    )
    .0
}

fn create_mint(svm: &mut LiteSVM, payer: &Keypair) -> Pubkey {
    let mint = Keypair::new();
    let rent = svm.minimum_balance_for_rent_exemption(82);
    let create = system_instruction::create_account(
        &payer.pubkey(),
        &mint.pubkey(),
        rent,
        82,
        &TOKEN_PROGRAM_ID,
    );
    let mut data = vec![20u8, 0]; // InitializeMint2, 0 decimals
    data.extend_from_slice(payer.pubkey().as_ref());
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

fn mint_to(svm: &mut LiteSVM, payer: &Keypair, mint: &Pubkey, dest: &Pubkey, amount: u64) {
    let mut data = vec![7u8];
    data.extend_from_slice(&amount.to_le_bytes());
    let ix = Instruction {
        program_id: TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*mint, false),
            AccountMeta::new(*dest, false),
            AccountMeta::new_readonly(payer.pubkey(), true),
        ],
        data,
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[payer],
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx).expect("mint_to");
}

struct World {
    svm: LiteSVM,
    payer: Keypair,
    caller: Keypair,
    cold: Keypair,
    gacha_wallet: Pubkey,
    usdc_mint: Pubkey,
    vault: Pubkey,
    config: Pubkey,
}

fn setup() -> World {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(
        PROGRAM_ID,
        common::program_so_path(),
    )
    .expect("run `anchor build` first");

    let payer = Keypair::new();
    let caller = Keypair::new(); // a RANDOM third party — sweep is permissionless
    let cold = Keypair::new();
    let hot = Keypair::new();
    for kp in [&payer, &caller, &cold, &hot] {
        svm.airdrop(&kp.pubkey(), 10_000_000_000).unwrap();
    }
    let gacha_wallet = Pubkey::new_unique();
    let usdc_mint = create_mint(&mut svm, &payer);
    let (vault, _) =
        Pubkey::find_program_address(&[VAULT_SEED, cold.pubkey().as_ref()], &PROGRAM_ID);

    let (config, bump) = Pubkey::find_program_address(&[CONFIG_SEED], &PROGRAM_ID);
    let cfg = Config {
        admin: Pubkey::new_unique(),
        usdc_mint,
        gacha_wallet,
        gacha_usdc_account: Pubkey::new_unique(),
        fee_usdc_account: Pubkey::new_unique(),
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
        data: instruction::InitVault {
            hot_delegate: hot.pubkey(),
            per_spin_cap: 1,
            daily_cap: 1,
        }
        .data(),
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[&payer, &cold],
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx).expect("init_vault");

    World { svm, payer, caller, cold, gacha_wallet, usdc_mint, vault, config }
}

fn sweep_ix(w: &World, token: Pubkey) -> Instruction {
    Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::SweepPrizeAta {
            config: w.config,
            vault: w.vault,
            token,
            rent_destination: w.gacha_wallet,
            token_program: TOKEN_PROGRAM_ID,
        }
        .to_account_metas(None),
        data: instruction::SweepPrizeAta {}.data(),
    }
}

fn send(w: &mut World, ix: Instruction) -> Result<Vec<String>, String> {
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&w.caller.pubkey()),
        &[&w.caller],
        w.svm.latest_blockhash(),
    );
    w.svm
        .send_transaction(tx)
        .map(|m| m.logs)
        .map_err(|f| f.meta.logs.join("\n"))
}

/// A random caller sweeps an empty prize ATA; rent lands on the CC wallet.
#[test]
fn sweep_closes_empty_prize_ata_rent_to_cc() {
    let mut w = setup();
    let prize_mint = create_mint(&mut w.svm, &w.payer);
    let payer = w.payer.insecure_clone();
    let prize_ata = create_ata(&mut w.svm, &payer, &w.vault, &prize_mint);
    let rent = w.svm.get_account(&prize_ata).unwrap().lamports;
    assert!(rent > 0);
    let vault_before = w.svm.get_account(&w.vault).unwrap().lamports;

    let ix = sweep_ix(&w, prize_ata);
    send(&mut w, ix).expect("permissionless sweep");

    assert!(w.svm.get_account(&prize_ata).map_or(true, |a| a.data.is_empty()));
    let cc = w.svm.get_account(&w.gacha_wallet).map_or(0, |a| a.lamports);
    assert_eq!(cc, rent, "rent landed on the config-pinned CC wallet");
    assert_eq!(
        w.svm.get_account(&w.vault).unwrap().lamports,
        vault_before,
        "the vault only passes the rent through"
    );
}

/// A non-empty account must be refused.
#[test]
fn sweep_rejects_non_empty_account() {
    let mut w = setup();
    let prize_mint = create_mint(&mut w.svm, &w.payer);
    let payer = w.payer.insecure_clone();
    let prize_ata = create_ata(&mut w.svm, &payer, &w.vault, &prize_mint);
    mint_to(&mut w.svm, &payer, &prize_mint, &prize_ata, 1);

    let ix = sweep_ix(&w, prize_ata);
    let err = send(&mut w, ix).expect_err("non-empty must fail");
    assert!(err.contains("TokenAccountNotEmpty"), "got:\n{err}");
}

/// Dust on the prize ATA must NOT block the sweep (one lamport from any
/// stranger would otherwise strand CC's rent forever). CC receives exactly the
/// rent it fronted; the excess stays in the vault, where the cold owner reaches
/// it with `withdraw_sol`.
#[test]
fn sweep_forwards_only_rent_and_leaves_excess_in_the_vault() {
    let mut w = setup();
    let prize_mint = create_mint(&mut w.svm, &w.payer);
    let payer = w.payer.insecure_clone();
    let prize_ata = create_ata(&mut w.svm, &payer, &w.vault, &prize_mint);
    let rent = w.svm.get_account(&prize_ata).unwrap().lamports;

    // A griefer (or a stray transfer) tops the ATA up above its rent floor.
    const DUST: u64 = 1_000_000;
    let topup = system_instruction::transfer(&payer.pubkey(), &prize_ata, DUST);
    let tx = Transaction::new_signed_with_payer(
        &[topup],
        Some(&payer.pubkey()),
        &[&payer],
        w.svm.latest_blockhash(),
    );
    w.svm.send_transaction(tx).expect("top up ata lamports");

    let vault_before = w.svm.get_account(&w.vault).unwrap().lamports;
    let ix = sweep_ix(&w, prize_ata);
    let logs = send(&mut w, ix).expect("dust must not block the sweep");

    let cc = w.svm.get_account(&w.gacha_wallet).map_or(0, |a| a.lamports);
    assert_eq!(cc, rent, "CC gets exactly the rent it fronted, not the dust");
    let vault_after = w.svm.get_account(&w.vault).unwrap().lamports;
    assert_eq!(
        vault_after - vault_before,
        DUST,
        "the excess stays with the vault owner"
    );

    // The emitted event must agree with the lamports that actually moved —
    // it is what CC's cron reconciles against, and nothing else decodes it.
    let ev: PrizeAtaSwept =
        common::decode_event(&logs).expect("PrizeAtaSwept was emitted and decodes");
    assert_eq!(ev.vault, w.vault);
    assert_eq!(ev.token_account, prize_ata);
    assert_eq!(
        ev.rent_refund, rent,
        "rent_refund must report what CC received, not the account's whole balance"
    );
}

/// The rent destination is the only access control on the only signer-less
/// instruction: a caller substituting their own wallet must be refused.
#[test]
fn sweep_rejects_foreign_rent_destination() {
    let mut w = setup();
    let prize_mint = create_mint(&mut w.svm, &w.payer);
    let payer = w.payer.insecure_clone();
    let prize_ata = create_ata(&mut w.svm, &payer, &w.vault, &prize_mint);
    let rent = w.svm.get_account(&prize_ata).unwrap().lamports;

    let mut ix = sweep_ix(&w, prize_ata);
    let thief = w.caller.pubkey();
    ix.accounts = accounts::SweepPrizeAta {
        config: w.config,
        vault: w.vault,
        token: prize_ata,
        rent_destination: thief,
        token_program: TOKEN_PROGRAM_ID,
    }
    .to_account_metas(None);

    let err = send(&mut w, ix).expect_err("foreign rent destination must fail");
    assert!(err.contains("ConstraintAddress"), "got:\n{err}");
    assert_eq!(
        w.svm.get_account(&prize_ata).unwrap().lamports,
        rent,
        "the ATA survives untouched"
    );
}

/// The two GC paths differ on purpose: `close_token_account` refuses an account
/// that still carries an SPL delegate, so the sweep is the ONLY way to reclaim
/// such an account. Pin that asymmetry as an executable spec.
#[test]
fn sweep_clears_a_delegated_empty_ata_that_close_token_account_refuses() {
    let mut w = setup();
    let prize_mint = create_mint(&mut w.svm, &w.payer);
    let payer = w.payer.insecure_clone();
    let prize_ata = create_ata(&mut w.svm, &payer, &w.vault, &prize_mint);

    // Write a stuck SPL delegate directly: the vault PDA is the only authority
    // that could approve one, and the program exposes no instruction for it.
    let mut acct = w.svm.get_account(&prize_ata).unwrap();
    acct.data[72..76].copy_from_slice(&1u32.to_le_bytes()); // COption::Some
    acct.data[76..108].copy_from_slice(Pubkey::new_unique().as_ref());
    w.svm.set_account(prize_ata, acct).unwrap();

    let close_ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::CloseTokenAccountCtx {
            vault: w.vault,
            cold_owner: w.cold.pubkey(),
            token: prize_ata,
            token_program: TOKEN_PROGRAM_ID,
        }
        .to_account_metas(None),
        data: instruction::CloseTokenAccount {}.data(),
    };
    let cold = w.cold.insecure_clone();
    let tx = Transaction::new_signed_with_payer(
        &[close_ix],
        Some(&cold.pubkey()),
        &[&cold],
        w.svm.latest_blockhash(),
    );
    let err = w
        .svm
        .send_transaction(tx)
        .expect_err("close_token_account must refuse a delegated account")
        .meta
        .logs
        .join("\n");
    assert!(err.contains("DelegateStillSet"), "got:\n{err}");

    let ix = sweep_ix(&w, prize_ata);
    send(&mut w, ix).expect("the sweep is the escape hatch for a stuck delegate");
    assert!(w.svm.get_account(&prize_ata).map_or(true, |a| a.data.is_empty()));
}

/// The vault's USDC ATA is off-limits even when empty.
#[test]
fn sweep_rejects_usdc_account() {
    let mut w = setup();
    let payer = w.payer.insecure_clone();
    let usdc_mint = w.usdc_mint;
    let usdc_ata = create_ata(&mut w.svm, &payer, &w.vault, &usdc_mint);

    let ix = sweep_ix(&w, usdc_ata);
    let err = send(&mut w, ix).expect_err("USDC ATA must fail");
    assert!(err.contains("CannotSweepUsdc"), "got:\n{err}");
}

/// The account the live buyback slot points at cannot be swept away.
#[test]
fn sweep_rejects_live_buyback_token() {
    let mut w = setup();
    let prize_mint = create_mint(&mut w.svm, &w.payer);
    let payer = w.payer.insecure_clone();
    let prize_ata = create_ata(&mut w.svm, &payer, &w.vault, &prize_mint);

    // Occupy the slot pointing at this exact account (state written directly).
    let acct = w.svm.get_account(&w.vault).unwrap();
    let mut vault: Vault =
        anchor_lang::AccountDeserialize::try_deserialize(&mut acct.data.as_slice()).unwrap();
    vault.live_buyback_mint = Some(prize_mint);
    vault.live_buyback_token = Some(prize_ata);
    let mut data = Vec::new();
    vault.try_serialize(&mut data).unwrap();
    w.svm
        .set_account(
            w.vault,
            Account { lamports: acct.lamports, data, owner: PROGRAM_ID, executable: false, rent_epoch: 0 },
        )
        .unwrap();

    let ix = sweep_ix(&w, prize_ata);
    let err = send(&mut w, ix).expect_err("live slot token must fail");
    assert!(err.contains("BuybackStillActive"), "got:\n{err}");

    // Sanity: the cold owner's own GC path refuses it too.
    let ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::CloseTokenAccountCtx {
            vault: w.vault,
            cold_owner: w.cold.pubkey(),
            token: prize_ata,
            token_program: TOKEN_PROGRAM_ID,
        }
        .to_account_metas(None),
        data: instruction::CloseTokenAccount {}.data(),
    };
    let cold = w.cold.insecure_clone();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&cold.pubkey()),
        &[&cold],
        w.svm.latest_blockhash(),
    );
    assert!(w.svm.send_transaction(tx).is_err());
}
