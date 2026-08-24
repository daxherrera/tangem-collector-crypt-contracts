//! Metaplex Core withdrawal tests on real bytecode (LiteSVM).
//!
//! Runs the REAL mainnet mpl-core program against REAL CC Core prize and
//! collection account dumps (tests/fixtures/cc_core_*.json), with the
//! asset's owner field patched to this test's vault PDA. This exercises
//! withdraw_core exactly as it runs in production: CC's collection plugins
//! (PermanentTransferDelegate, Royalties, …) are all present and evaluated.
//!
//! Run `anchor build` first so target/deploy/tangem_gacha_vault.so exists.

mod common;

use anchor_lang::{AccountSerialize, InstructionData, ToAccountMetas};
use base64::Engine;
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
use tangem_gacha_vault::{accounts, instruction, Config, CONFIG_SEED, MPL_CORE_ID, VAULT_SEED};

const PROGRAM_ID: Pubkey = tangem_gacha_vault::ID;
const CC_CORE_COLLECTION: Pubkey = pubkey!("CCryptUfeFSZ3Fgc9FLeKrhLVAP67FSqi1GuVoj9CRac");
const CC_CORE_ASSET: Pubkey = pubkey!("13fCVtpxtzN8mv8jERe6Ev7rSuXM4nbSwFGhmauvKB7b");
const TOKEN_PROGRAM_ID: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const ATA_PROGRAM_ID: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

fn fixture(name: &str) -> String {
    format!(
        "{}/../../tests/fixtures/{}",
        env!("CARGO_MANIFEST_DIR"),
        name
    )
}

/// Loads a `solana account --output json` dump into the SVM, optionally
/// patching the AssetV1 owner field (bytes 1..33) to `patch_owner`.
fn load_account_dump(svm: &mut LiteSVM, path: &str, address: Pubkey, patch_owner: Option<Pubkey>) {
    let raw = std::fs::read_to_string(path).expect("account dump");
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let acc = &json["account"];
    let mut data = base64::engine::general_purpose::STANDARD
        .decode(acc["data"][0].as_str().unwrap())
        .unwrap();
    if let Some(owner) = patch_owner {
        assert_eq!(data[0], 1, "AssetV1 key byte");
        data[1..33].copy_from_slice(owner.as_ref());
    }
    let owner_program: Pubkey = acc["owner"].as_str().unwrap().parse().unwrap();
    svm.set_account(
        address,
        Account {
            lamports: acc["lamports"].as_u64().unwrap(),
            data,
            owner: owner_program,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

fn asset_owner(svm: &LiteSVM) -> Pubkey {
    let acc = svm.get_account(&CC_CORE_ASSET).unwrap();
    Pubkey::new_from_array(acc.data[1..33].try_into().unwrap())
}

struct World {
    svm: LiteSVM,
    payer: Keypair,
    cold: Keypair,
    hot: Keypair,
    vault: Pubkey,
}

fn setup() -> World {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(
        PROGRAM_ID,
        common::program_so_path(),
    )
    .expect("run `anchor build` first");
    svm.add_program_from_file(MPL_CORE_ID, fixture("mpl_core.so"))
        .expect("mpl_core.so fixture");

    let payer = Keypair::new();
    let cold = Keypair::new();
    let hot = Keypair::new();
    for kp in [&payer, &cold, &hot] {
        svm.airdrop(&kp.pubkey(), 10_000_000_000).unwrap();
    }
    let (vault, _) =
        Pubkey::find_program_address(&[VAULT_SEED, cold.pubkey().as_ref()], &PROGRAM_ID);

    // The REAL CC collection as-is; the REAL CC asset with owner = our vault.
    load_account_dump(&mut svm, &fixture("cc_core_collection.json"), CC_CORE_COLLECTION, None);
    load_account_dump(&mut svm, &fixture("cc_core_asset.json"), CC_CORE_ASSET, Some(vault));

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
            per_spin_cap: 300_000_000,
            daily_cap: 1_000_000_000,
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

    World { svm, payer, cold, hot, vault }
}

fn withdraw_ix(w: &World, destination: Pubkey) -> Instruction {
    Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::WithdrawCore {
            vault: w.vault,
            cold_owner: w.cold.pubkey(),
            payer: w.payer.pubkey(),
            asset: CC_CORE_ASSET,
            collection: CC_CORE_COLLECTION,
            destination_owner: destination,
            mpl_core_program: MPL_CORE_ID,
        }
        .to_account_metas(None),
        data: instruction::WithdrawCore {}.data(),
    }
}

#[test]
fn withdraw_core_moves_real_cc_asset_to_cold_wallet() {
    let mut w = setup();
    assert_eq!(asset_owner(&w.svm), w.vault, "prize starts vault-owned");

    let tx = Transaction::new_signed_with_payer(
        &[withdraw_ix(&w, w.cold.pubkey())],
        Some(&w.payer.pubkey()),
        &[&w.payer, &w.cold],
        w.svm.latest_blockhash(),
    );
    w.svm
        .send_transaction(tx)
        .expect("withdraw_core against the real mpl-core + real CC collection plugins");
    assert_eq!(
        asset_owner(&w.svm),
        w.cold.pubkey(),
        "Core asset owner is now the cold wallet"
    );
}

#[test]
fn withdraw_core_rejects_non_cold_signer() {
    let mut w = setup();
    let mut ix = withdraw_ix(&w, w.hot.pubkey());
    ix.accounts[1].pubkey = w.hot.pubkey(); // impostor in the cold_owner slot
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&w.payer.pubkey()),
        &[&w.payer, &w.hot],
        w.svm.latest_blockhash(),
    );
    match w.svm.send_transaction(tx) {
        Ok(_) => panic!("hot key must not withdraw a Core prize"),
        Err(f) => {
            let logs = f.meta.logs.join("\n");
            assert!(
                logs.contains("ConstraintSeeds"),
                "expected the vault-seeds check to reject the impostor, got:\n{logs}"
            );
        }
    }
    assert_eq!(asset_owner(&w.svm), w.vault, "prize stays in the vault");
}

/// The mpl-core program itself must refuse to move an asset the vault does
/// not own (defense in depth below our anchor constraints): patch the asset
/// owner to a stranger and try to withdraw it through the vault.
#[test]
fn withdraw_core_fails_when_vault_is_not_the_owner() {
    let mut w = setup();
    let stranger = Keypair::new();
    load_account_dump(
        &mut w.svm,
        &fixture("cc_core_asset.json"),
        CC_CORE_ASSET,
        Some(stranger.pubkey()),
    );

    let tx = Transaction::new_signed_with_payer(
        &[withdraw_ix(&w, w.cold.pubkey())],
        Some(&w.payer.pubkey()),
        &[&w.payer, &w.cold],
        w.svm.latest_blockhash(),
    );
    match w.svm.send_transaction(tx) {
        Ok(_) => panic!("mpl-core must reject a transfer signed by a non-owner"),
        Err(f) => {
            let logs = f.meta.logs.join("\n");
            // Pin the failure to the mpl-core CPI itself — a failure of the
            // vault program (wrong constraint, bad accounts) must not pass.
            assert!(
                logs.contains(&format!("Program {MPL_CORE_ID} failed")),
                "expected the rejection to originate from mpl-core, got:\n{logs}"
            );
        }
    }
    assert_eq!(asset_owner(&w.svm), stranger.pubkey());
}

// ---------------------------------------------------------------------------
// buyback_core — the atomic in-program buyback of a Core prize
// ---------------------------------------------------------------------------

fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), TOKEN_PROGRAM_ID.as_ref(), mint.as_ref()],
        &ATA_PROGRAM_ID,
    )
    .0
}

fn spl_amount(svm: &LiteSVM, token: &Pubkey) -> u64 {
    let acct = svm.get_account(token).unwrap();
    u64::from_le_bytes(acct.data[64..72].try_into().unwrap())
}

/// USDC world for the buyback tests: config PDA (gacha_wallet = `cc`),
/// 6-decimals mint, vault + CC ATAs, CC funded with 1000 USDC.
fn install_usdc_world(w: &mut World, cc: &Keypair) -> (Pubkey, Pubkey, Pubkey, Pubkey) {
    let payer = w.payer.insecure_clone();

    let mint_kp = Keypair::new();
    let rent = w.svm.minimum_balance_for_rent_exemption(82);
    let create = system_instruction::create_account(
        &payer.pubkey(),
        &mint_kp.pubkey(),
        rent,
        82,
        &TOKEN_PROGRAM_ID,
    );
    let mut data = vec![20u8, 6]; // InitializeMint2, 6 decimals
    data.extend_from_slice(payer.pubkey().as_ref());
    data.push(0);
    let init = Instruction {
        program_id: TOKEN_PROGRAM_ID,
        accounts: vec![AccountMeta::new(mint_kp.pubkey(), false)],
        data,
    };
    let tx = Transaction::new_signed_with_payer(
        &[create, init],
        Some(&payer.pubkey()),
        &[&payer, &mint_kp],
        w.svm.latest_blockhash(),
    );
    w.svm.send_transaction(tx).expect("create usdc mint");
    let usdc_mint = mint_kp.pubkey();

    let (config_pda, bump) = Pubkey::find_program_address(&[CONFIG_SEED], &PROGRAM_ID);
    let cfg = Config {
        admin: Pubkey::new_unique(),
        usdc_mint,
        gacha_wallet: cc.pubkey(),
        gacha_usdc_account: Pubkey::new_unique(),
        fee_usdc_account: Pubkey::new_unique(),
        fee_bps: 0,
        paused: false,
        _reserved: false,
        allow_buyback_delegation: true,
        bump,
    };
    let mut data = Vec::new();
    cfg.try_serialize(&mut data).unwrap();
    let lamports = w.svm.minimum_balance_for_rent_exemption(data.len());
    w.svm
        .set_account(
            config_pda,
            Account { lamports, data, owner: PROGRAM_ID, executable: false, rent_epoch: 0 },
        )
        .unwrap();

    let mut atas = Vec::new();
    for owner in [w.vault, cc.pubkey()] {
        let addr = ata(&owner, &usdc_mint);
        let ix = Instruction {
            program_id: ATA_PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new(addr, false),
                AccountMeta::new_readonly(owner, false),
                AccountMeta::new_readonly(usdc_mint, false),
                AccountMeta::new_readonly(system_program::ID, false),
                AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
            ],
            data: vec![1], // CreateIdempotent
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&payer.pubkey()),
            &[&payer],
            w.svm.latest_blockhash(),
        );
        w.svm.send_transaction(tx).expect("create ata");
        atas.push(addr);
    }
    let (vault_usdc, cc_usdc) = (atas[0], atas[1]);

    let mut data = vec![7u8]; // MintTo
    data.extend_from_slice(&1_000_000_000u64.to_le_bytes());
    let ix = Instruction {
        program_id: TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(usdc_mint, false),
            AccountMeta::new(cc_usdc, false),
            AccountMeta::new_readonly(payer.pubkey(), true),
        ],
        data,
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[&payer],
        w.svm.latest_blockhash(),
    );
    w.svm.send_transaction(tx).expect("fund cc usdc");

    (config_pda, usdc_mint, vault_usdc, cc_usdc)
}

fn buyback_core_ix(
    w: &World,
    config: Pubkey,
    cc: &Keypair,
    destination_owner: Pubkey,
    usdc_mint: Pubkey,
    cc_usdc: Pubkey,
    price: u64,
) -> Instruction {
    Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::BuybackCore {
            config,
            vault: w.vault,
            hot_delegate: w.hot.pubkey(),
            cc_authority: cc.pubkey(),
            destination_owner,
            usdc_mint,
            cc_usdc,
            vault_usdc: ata(&w.vault, &usdc_mint),
            asset: CC_CORE_ASSET,
            collection: CC_CORE_COLLECTION,
            mpl_core_program: MPL_CORE_ID,
            token_program: TOKEN_PROGRAM_ID,
        }
        .to_account_metas(None),
        data: instruction::BuybackCore {
            price,
            memo: "tangem-11111111-2222:buyback".to_string(),
        }
        .data(),
    }
}

/// Happy path against the REAL CC Core asset + collection plugins: CC pays,
/// the vault gets the USDC, CC becomes the asset owner — one instruction.
#[test]
fn buyback_core_atomic_swap() {
    let mut w = setup();
    let cc = Keypair::new();
    w.svm.airdrop(&cc.pubkey(), 10_000_000_000).unwrap();
    let (config, usdc_mint, vault_usdc, cc_usdc) = install_usdc_world(&mut w, &cc);
    assert_eq!(asset_owner(&w.svm), w.vault, "prize starts vault-owned");

    let price = 150_000_000; // 150 USDC
    // CC's prizes live on rotating prize wallets — the destination is a
    // different key than the signing operator wallet, and that must work.
    let prize_wallet = Pubkey::new_unique();
    let ix = buyback_core_ix(&w, config, &cc, prize_wallet, usdc_mint, cc_usdc, price);
    let hot = w.hot.insecure_clone();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&cc.pubkey()),
        &[&cc, &hot],
        w.svm.latest_blockhash(),
    );
    w.svm
        .send_transaction(tx)
        .expect("buyback_core against the real mpl-core + CC collection plugins");

    assert_eq!(asset_owner(&w.svm), prize_wallet, "Core asset went to CC's prize wallet, not the signer");
    assert_eq!(spl_amount(&w.svm, &vault_usdc), price, "refund landed in the vault");
    assert_eq!(spl_amount(&w.svm, &cc_usdc), 1_000_000_000 - price);
}

/// Only the config-pinned CC wallet can execute a Core buyback.
#[test]
fn buyback_core_rejects_non_cc_signer() {
    let mut w = setup();
    let cc = Keypair::new();
    w.svm.airdrop(&cc.pubkey(), 10_000_000_000).unwrap();
    let (config, usdc_mint, _, _) = install_usdc_world(&mut w, &cc);

    let stranger = Keypair::new();
    w.svm.airdrop(&stranger.pubkey(), 10_000_000_000).unwrap();
    let stranger_usdc = {
        let payer = w.payer.insecure_clone();
        let addr = ata(&stranger.pubkey(), &usdc_mint);
        let ix = Instruction {
            program_id: ATA_PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new(addr, false),
                AccountMeta::new_readonly(stranger.pubkey(), false),
                AccountMeta::new_readonly(usdc_mint, false),
                AccountMeta::new_readonly(system_program::ID, false),
                AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
            ],
            data: vec![1],
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&payer.pubkey()),
            &[&payer],
            w.svm.latest_blockhash(),
        );
        w.svm.send_transaction(tx).expect("stranger ata");
        addr
    };

    let mut ix = buyback_core_ix(&w, config, &cc, cc.pubkey(), usdc_mint, stranger_usdc, 1_000_000);
    ix.accounts[3].pubkey = stranger.pubkey(); // impostor in the cc_authority slot
    let hot = w.hot.insecure_clone();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&stranger.pubkey()),
        &[&stranger, &hot],
        w.svm.latest_blockhash(),
    );
    match w.svm.send_transaction(tx) {
        Ok(_) => panic!("non-CC signer must not execute a Core buyback"),
        Err(f) => {
            let logs = f.meta.logs.join("\n");
            assert!(
                logs.contains("ConstraintAddress"),
                "expected the gacha-wallet pin to reject the impostor, got:\n{logs}"
            );
        }
    }
    assert_eq!(asset_owner(&w.svm), w.vault, "prize stays in the vault");
}

/// Without the hot key's consent co-signature CC alone cannot buy the asset.
#[test]
fn buyback_core_rejects_without_hot_consent() {
    let mut w = setup();
    let cc = Keypair::new();
    w.svm.airdrop(&cc.pubkey(), 10_000_000_000).unwrap();
    let (config, usdc_mint, _, cc_usdc) = install_usdc_world(&mut w, &cc);

    let stranger = Keypair::new();
    w.svm.airdrop(&stranger.pubkey(), 1_000_000_000).unwrap();
    let mut ix = buyback_core_ix(&w, config, &cc, cc.pubkey(), usdc_mint, cc_usdc, 1_000_000);
    ix.accounts[2].pubkey = stranger.pubkey(); // impostor in the hot slot
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&cc.pubkey()),
        &[&cc, &stranger],
        w.svm.latest_blockhash(),
    );
    match w.svm.send_transaction(tx) {
        Ok(_) => panic!("Core buyback without hot consent must fail"),
        Err(f) => {
            let logs = f.meta.logs.join("\n");
            assert!(
                logs.contains("ConstraintHasOne"),
                "expected the hot-delegate check to reject, got:\n{logs}"
            );
        }
    }
    assert_eq!(asset_owner(&w.svm), w.vault, "prize stays in the vault");
}

/// The core economic guarantee, exercised on the Core path: a failed payment
/// leg (price exceeds CC's balance) reverts the whole instruction — the
/// asset never leaves the vault.
#[test]
fn buyback_core_reverts_atomically_when_payment_fails() {
    let mut w = setup();
    let cc = Keypair::new();
    w.svm.airdrop(&cc.pubkey(), 10_000_000_000).unwrap();
    let (config, usdc_mint, vault_usdc, cc_usdc) = install_usdc_world(&mut w, &cc);

    // CC holds 1000 USDC; demand more than it can pay.
    let ix = buyback_core_ix(&w, config, &cc, cc.pubkey(), usdc_mint, cc_usdc, 2_000_000_000);
    let hot = w.hot.insecure_clone();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&cc.pubkey()),
        &[&cc, &hot],
        w.svm.latest_blockhash(),
    );
    match w.svm.send_transaction(tx) {
        Ok(_) => panic!("underfunded Core buyback must fail"),
        Err(f) => {
            let logs = f.meta.logs.join("\n");
            assert!(logs.contains("insufficient funds"), "got:\n{logs}");
        }
    }
    assert_eq!(asset_owner(&w.svm), w.vault, "asset never left the vault");
    assert_eq!(spl_amount(&w.svm, &vault_usdc), 0, "no partial refund");
}

/// Paused program blocks Core buybacks; a zero price is refused outright.
#[test]
fn buyback_core_rejects_paused_and_zero_price() {
    let mut w = setup();
    let cc = Keypair::new();
    w.svm.airdrop(&cc.pubkey(), 10_000_000_000).unwrap();
    let (config, usdc_mint, _, cc_usdc) = install_usdc_world(&mut w, &cc);

    let ix = buyback_core_ix(&w, config, &cc, cc.pubkey(), usdc_mint, cc_usdc, 0);
    let hot = w.hot.insecure_clone();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&cc.pubkey()),
        &[&cc, &hot],
        w.svm.latest_blockhash(),
    );
    match w.svm.send_transaction(tx) {
        Ok(_) => panic!("zero-price Core buyback must fail"),
        Err(f) => {
            let logs = f.meta.logs.join("\n");
            assert!(logs.contains("ZeroAmount"), "got:\n{logs}");
        }
    }

    // Flip paused=true (same pins otherwise) and retry with a real price.
    let (config_pda, bump) = Pubkey::find_program_address(&[CONFIG_SEED], &PROGRAM_ID);
    let cfg = Config {
        admin: Pubkey::new_unique(),
        usdc_mint,
        gacha_wallet: cc.pubkey(),
        gacha_usdc_account: Pubkey::new_unique(),
        fee_usdc_account: Pubkey::new_unique(),
        fee_bps: 0,
        paused: true,
        _reserved: false,
        allow_buyback_delegation: true,
        bump,
    };
    let mut data = Vec::new();
    cfg.try_serialize(&mut data).unwrap();
    let lamports = w.svm.minimum_balance_for_rent_exemption(data.len());
    w.svm
        .set_account(
            config_pda,
            Account { lamports, data, owner: PROGRAM_ID, executable: false, rent_epoch: 0 },
        )
        .unwrap();

    let ix = buyback_core_ix(&w, config, &cc, cc.pubkey(), usdc_mint, cc_usdc, 1_000_000);
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&cc.pubkey()),
        &[&cc, &hot],
        w.svm.latest_blockhash(),
    );
    match w.svm.send_transaction(tx) {
        Ok(_) => panic!("paused Core buyback must fail"),
        Err(f) => {
            let logs = f.meta.logs.join("\n");
            assert!(logs.contains("Paused"), "got:\n{logs}");
        }
    }
    assert_eq!(asset_owner(&w.svm), w.vault, "asset stays in the vault");
}
