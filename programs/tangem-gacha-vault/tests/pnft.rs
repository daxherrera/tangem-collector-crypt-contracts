//! pNFT integration tests on the real compiled bytecode (LiteSVM, in-process).
//!
//! Exercises the buyback delegation lifecycle and the cold-key pNFT withdrawal
//! against the REAL Metaplex programs and the REAL Collector Crypt rule set,
//! all dumped from mainnet into tests/fixtures/ (see Anchor.toml for the dump
//! commands). Prize NFTs here are what CC ships: ProgrammableNonFungible,
//! frozen token accounts, Metaplex Foundation Rule Set.
//!
//! Run `anchor build` first so target/deploy/tangem_gacha_vault.so exists.

mod common;

use common::{
    cc_ed25519_ix, cc_policy_pda, cc_quote_digest, cc_quote_marker_pda, cc_rent_vault_pda,
    plant_cc_policy, CcLane,
};

use anchor_lang::{AccountDeserialize, AccountSerialize, InstructionData, ToAccountMetas};
use base64::Engine;
use litesvm::LiteSVM;
use mpl_token_metadata::accounts::{MasterEdition, Metadata, TokenRecord};
use mpl_token_metadata::instructions::{CreateV1Builder, MintV1Builder, TransferV1Builder};
use mpl_token_metadata::types::{Collection, Creator, PrintSupply, TokenDelegateRole, TokenStandard};
use solana_sdk::{
    account::Account,
    address_lookup_table::AddressLookupTableAccount,
    compute_budget::ComputeBudgetInstruction,
    instruction::{AccountMeta, Instruction},
    message::{v0, VersionedMessage},
    pubkey,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction, system_program, sysvar,
    transaction::{Transaction, VersionedTransaction},
};
use tangem_gacha_vault::{
    accounts, instruction, Config, Vault, CC_BUYBACK_ID, CONFIG_SEED, VAULT_SEED,
};

const PROGRAM_ID: Pubkey = tangem_gacha_vault::ID;
const AUTH_RULES_PROGRAM_ID: Pubkey = pubkey!("auth9SigNpDKz4sJJ1DfCTuZrZNSAgh9sFD3rboVmgg");
/// Metaplex Foundation Rule Set — the ruleset CC prize cards actually carry.
const CC_RULE_SET: Pubkey = pubkey!("eBJLFYPxJmMGKuFwpDWkzxZeUrad92kZRC5BJLpzyT9");
const TOKEN_PROGRAM_ID: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const ATA_PROGRAM_ID: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
const MEMO_PROGRAM_ID: Pubkey = pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");
/// Solana packet limit; the serialized transaction must stay under this.
const PACKET_DATA_SIZE: usize = 1232;
/// The reconciliation key CC's matcher greps for — carried BOTH as the
/// buyback instruction argument (recorded in BuybackExecuted) and as a
/// top-level SPL Memo instruction in the same transaction.
const CC_BUYBACK_MEMO: &str = "dev-00000000-1111-2222-3333-444444444444:buyback";

fn fixture(name: &str) -> String {
    format!(
        "{}/../../tests/fixtures/{}",
        env!("CARGO_MANIFEST_DIR"),
        name
    )
}

fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), TOKEN_PROGRAM_ID.as_ref(), mint.as_ref()],
        &ATA_PROGRAM_ID,
    )
    .0
}

/// Loads the mainnet dump of the CC rule set account (solana account
/// --output json) into the SVM at its real address.
fn load_rule_set(svm: &mut LiteSVM) {
    let raw = std::fs::read_to_string(fixture("cc_rule_set.json")).expect("cc_rule_set.json");
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let acc = &json["account"];
    let data = base64::engine::general_purpose::STANDARD
        .decode(acc["data"][0].as_str().unwrap())
        .unwrap();
    svm.set_account(
        CC_RULE_SET,
        Account {
            lamports: acc["lamports"].as_u64().unwrap(),
            data,
            owner: AUTH_RULES_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

/// Writes the global Config PDA directly (initialize_config's upgrade-authority
/// gate needs a BPFLoaderUpgradeable ProgramData account, which litesvm's
/// add_program does not create; config initialization itself is covered by the
/// TS suite).
fn write_config(svm: &mut LiteSVM, allow_buyback_delegation: bool, paused: bool) -> Pubkey {
    let (config_pda, bump) = Pubkey::find_program_address(&[CONFIG_SEED], &PROGRAM_ID);
    let cfg = Config {
        admin: Pubkey::new_unique(),
        pending_admin: Pubkey::default(),
        usdc_mint: Pubkey::new_unique(),
        rent_destination: Pubkey::new_unique(),
        gacha_usdc_account: Pubkey::new_unique(),
        fee_usdc_account: Pubkey::new_unique(),
        fee_bps: 0,
        paused,
        _reserved: false,
        allow_buyback_delegation,
        bump,
        _padding: [0u8; 128],
    };
    let mut data = Vec::new();
    cfg.try_serialize(&mut data).unwrap();
    let lamports = svm.minimum_balance_for_rent_exemption(data.len());
    svm.set_account(
        config_pda,
        Account {
            lamports,
            data,
            owner: PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
    config_pda
}

struct World {
    svm: LiteSVM,
    /// Funds rent/fees on the Tangem side of the tests.
    payer: Keypair,
    cold: Keypair,
    hot: Keypair,
    /// The Collector Crypt side: prize-wallet owner, pNFT mint/update
    /// authority, and the memo co-signer (the GachaNgy... role).
    cc_operator: Keypair,
    vault: Pubkey,
    config: Pubkey,
}

/// PDAs derived for one prize pNFT held by the vault.
struct Prize {
    mint: Pubkey,
    token: Pubkey,
    metadata: Pubkey,
    edition: Pubkey,
    token_record: Pubkey,
}

fn setup(allow_buyback_delegation: bool) -> World {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(
        PROGRAM_ID,
        common::program_so_path(),
    )
    .expect("load program .so — run `anchor build` first");
    svm.add_program_from_file(mpl_token_metadata::ID, fixture("mpl_token_metadata.so"))
        .expect("mpl_token_metadata.so fixture — see Anchor.toml dump commands");
    svm.add_program_from_file(AUTH_RULES_PROGRAM_ID, fixture("mpl_token_auth_rules.so"))
        .expect("mpl_token_auth_rules.so fixture — see Anchor.toml dump commands");
    load_rule_set(&mut svm);

    let payer = Keypair::new();
    let cold = Keypair::new();
    let hot = Keypair::new();
    let cc_operator = Keypair::new();
    for kp in [&payer, &cold, &hot, &cc_operator] {
        svm.airdrop(&kp.pubkey(), 10_000_000_000).unwrap();
    }

    let config = write_config(&mut svm, allow_buyback_delegation, false);
    let (vault, _) = Pubkey::find_program_address(&[VAULT_SEED, cold.pubkey().as_ref()], &PROGRAM_ID);

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

    World {
        svm,
        payer,
        cold,
        hot,
        cc_operator,
        vault,
        config,
    }
}

/// A memo instruction co-signed by the CC operator — the shape of the
/// `<slug>-<uuid>:send` / `:buyback` legs in CC's real transactions.
fn cc_memo_ix(w: &World, text: &str) -> Instruction {
    Instruction {
        program_id: MEMO_PROGRAM_ID,
        accounts: vec![AccountMeta::new_readonly(w.cc_operator.pubkey(), true)],
        data: text.as_bytes().to_vec(),
    }
}

/// Top-level SPL Memo instruction WITHOUT accounts (Memo v3 only verifies
/// the signers it is given) — the atomic buyback already carries CC's and
/// the hot key's signatures on the program instruction itself.
fn memo_ix(text: &str) -> Instruction {
    Instruction {
        program_id: MEMO_PROGRAM_ID,
        accounts: vec![],
        data: text.as_bytes().to_vec(),
    }
}

/// Delivers a CC-shaped prize into the vault the way the gacha actually does:
/// the card is PRE-MINTED into CC's prize wallet (pNFT, CC rule set, creators,
/// royalties), then the `:send` leg moves it — a CC-co-signed memo plus an
/// owner-signed TransferV1 with `altPlayerAddress` semantics (destination =
/// the vault PDA, which never signs). This also pins the design premise that
/// an off-curve PDA can RECEIVE prizes.
fn mint_prize_to_vault(w: &mut World) -> Prize {
    mint_prize_to_vault_with_rule_set(w, Some(CC_RULE_SET))
}

/// Same delivery flow, but the rule set is parameterized: `None` mints a
/// ProgrammableNonFungible WITHOUT a rule set (still frozen, still pNFT
/// transfer rules) — a shape CC could ship and the withdraw path must handle
/// with both optional auth-rules slots absent.
fn mint_prize_to_vault_with_rule_set(w: &mut World, rule_set: Option<Pubkey>) -> Prize {
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let metadata = Metadata::find_pda(&mint).0;
    let edition = MasterEdition::find_pda(&mint).0;
    let cc_token = ata(&w.cc_operator.pubkey(), &mint);
    let cc_token_record = TokenRecord::find_pda(&mint, &cc_token).0;
    let token = ata(&w.vault, &mint);
    let token_record = TokenRecord::find_pda(&mint, &token).0;
    // The auth-rules program only rides along when a rule set is attached.
    let auth_rules_program = rule_set.map(|_| AUTH_RULES_PROGRAM_ID);

    // 1) CC pre-mints the card into its own prize wallet.
    let mut create_builder = CreateV1Builder::new();
    create_builder
        .metadata(metadata)
        .master_edition(Some(edition))
        .mint(mint, true)
        .authority(w.cc_operator.pubkey())
        .payer(w.cc_operator.pubkey())
        .update_authority(w.cc_operator.pubkey(), true)
        .name("CC Prize Card".to_string())
        .symbol("CC".to_string())
        .uri("https://collectorcrypt.example/card.json".to_string())
        .seller_fee_basis_points(500)
        .creators(vec![Creator {
            address: w.cc_operator.pubkey(),
            verified: true,
            share: 100,
        }])
        .collection(Collection {
            verified: false,
            key: Pubkey::new_unique(),
        })
        .token_standard(TokenStandard::ProgrammableNonFungible)
        .print_supply(PrintSupply::Zero)
        .spl_token_program(Some(TOKEN_PROGRAM_ID));
    if let Some(rs) = rule_set {
        create_builder.rule_set(rs);
    }
    let create_ix = create_builder.instruction();

    let mint_ix = MintV1Builder::new()
        .token(cc_token)
        .token_owner(Some(w.cc_operator.pubkey()))
        .metadata(metadata)
        .master_edition(Some(edition))
        .token_record(Some(cc_token_record))
        .mint(mint)
        .authority(w.cc_operator.pubkey())
        .payer(w.cc_operator.pubkey())
        .authorization_rules_program(auth_rules_program)
        .authorization_rules(rule_set)
        .amount(1)
        .instruction();

    let tx = Transaction::new_signed_with_payer(
        &[create_ix, mint_ix],
        Some(&w.cc_operator.pubkey()),
        &[&w.cc_operator, &mint_kp],
        w.svm.latest_blockhash(),
    );
    w.svm.send_transaction(tx).expect("create+mint pNFT into CC wallet");

    // 2) The `:send` leg: co-signed memo + owner TransferV1 to the vault PDA.
    let send_ix = TransferV1Builder::new()
        .token(cc_token)
        .token_owner(w.cc_operator.pubkey())
        .destination_token(token)
        .destination_owner(w.vault)
        .mint(mint)
        .metadata(metadata)
        .edition(Some(edition))
        .token_record(Some(cc_token_record))
        .destination_token_record(Some(token_record))
        .authority(w.cc_operator.pubkey())
        .payer(w.cc_operator.pubkey())
        .authorization_rules_program(auth_rules_program)
        .authorization_rules(rule_set)
        .amount(1)
        .instruction();
    // The :send TransferV1 lazily creates the vault's destination ATA +
    // TokenRecord, which pushes it near the 200k CU default — carry a
    // compute-budget bump (CC's real :send tx does the same).
    let tx = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(300_000),
            cc_memo_ix(w, "tangem-11111111-2222:send:rdeadbeef"),
            send_ix,
        ],
        Some(&w.cc_operator.pubkey()),
        &[&w.cc_operator],
        w.svm.latest_blockhash(),
    );
    w.svm.send_transaction(tx).expect(":send leg to the vault PDA");

    assert_eq!(spl_amount(w, &token), 1, "prize delivered to the vault");
    assert!(
        spl_is_frozen(w, &token),
        "pNFT token accounts are permanently frozen"
    );

    Prize {
        mint,
        token,
        metadata,
        edition,
        token_record,
    }
}

fn approve_buyback_ix(w: &World, p: &Prize) -> Instruction {
    Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::ApproveBuyback {
            config: w.config,
            vault: w.vault,
            hot_delegate: w.hot.pubkey(),
            nft_mint: p.mint,
            nft_token: p.token,
            metadata: p.metadata,
            edition: p.edition,
            token_record: p.token_record,
            authorization_rules: Some(CC_RULE_SET),
            authorization_rules_program: Some(AUTH_RULES_PROGRAM_ID),
            token_metadata_program: mpl_token_metadata::ID,
            sysvar_instructions: sysvar::instructions::ID,
            token_program: TOKEN_PROGRAM_ID,
            system_program: system_program::ID,
        }
        .to_account_metas(None),
        data: instruction::ApproveBuyback {}.data(),
    }
}

fn get_vault(w: &World) -> Vault {
    let acct = w.svm.get_account(&w.vault).unwrap();
    Vault::try_deserialize(&mut acct.data.as_slice()).unwrap()
}

/// None when the record account no longer exists — Token Metadata CLOSES the
/// source TokenRecord on TransferV1 (rent refunded to the tx payer).
fn token_record_of(w: &World, p: &Prize) -> Option<TokenRecord> {
    let acct = w.svm.get_account(&p.token_record)?;
    if acct.data.is_empty() {
        return None;
    }
    Some(TokenRecord::from_bytes(&acct.data).unwrap())
}

fn spl_amount(w: &World, token: &Pubkey) -> u64 {
    let acct = w.svm.get_account(token).unwrap();
    // SPL TokenAccount layout: amount is the u64 at offset 64.
    u64::from_le_bytes(acct.data[64..72].try_into().unwrap())
}

fn spl_is_frozen(w: &World, token: &Pubkey) -> bool {
    let acct = w.svm.get_account(token).unwrap();
    // SPL TokenAccount layout: state byte at offset 108 (2 = Frozen).
    acct.data[108] == 2
}

fn lamports_of(w: &World, key: &Pubkey) -> u64 {
    w.svm.get_account(key).map_or(0, |a| a.lamports)
}

/// A plain (non-ATA) SPL token account with owner = the vault, holding 0
/// tokens of `mint` — the "decoy" anyone can create without the vault signing.
fn create_decoy_vault_token_account(w: &mut World, mint: &Pubkey) -> Pubkey {
    let kp = Keypair::new();
    let len = 165;
    let rent = w.svm.minimum_balance_for_rent_exemption(len);
    let create = system_instruction::create_account(
        &w.payer.pubkey(),
        &kp.pubkey(),
        rent,
        len as u64,
        &TOKEN_PROGRAM_ID,
    );
    // spl-token InitializeAccount3: tag 18 + owner pubkey.
    let mut data = vec![18u8];
    data.extend_from_slice(w.vault.as_ref());
    let init = Instruction {
        program_id: TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(kp.pubkey(), false),
            AccountMeta::new_readonly(*mint, false),
        ],
        data,
    };
    let tx = Transaction::new_signed_with_payer(
        &[create, init],
        Some(&w.payer.pubkey()),
        &[&w.payer, &kp],
        w.svm.latest_blockhash(),
    );
    w.svm.send_transaction(tx).expect("create decoy token account");
    kp.pubkey()
}

fn assert_fails_with(w: &mut World, tx: Transaction, needle: &str, ctx: &str) {
    match w.svm.send_transaction(tx) {
        Ok(_) => panic!("{ctx}: expected failure containing {needle:?}, but tx succeeded"),
        Err(f) => {
            let logs = f.meta.logs.join("\n");
            assert!(
                logs.contains(needle),
                "{ctx}: expected {needle:?} in logs, got:\n{logs}\nerr: {:?}",
                f.err
            );
        }
    }
}

// ---------------------------------------------------------------------------
// approve_buyback
// ---------------------------------------------------------------------------

#[test]
fn approve_buyback_delegates_and_occupies_slot() {
    let mut w = setup(true);
    let p1 = mint_prize_to_vault(&mut w);
    let p2 = mint_prize_to_vault(&mut w);

    let tx = Transaction::new_signed_with_payer(
        &[approve_buyback_ix(&w, &p1)],
        Some(&w.hot.pubkey()),
        &[&w.hot],
        w.svm.latest_blockhash(),
    );
    w.svm.send_transaction(tx).expect("approve_buyback");

    // The hot key is now the Transfer delegate in the real TokenRecord, and
    // DelegateV1 pinned the rule-set revision (mainnet premise the buyback
    // template relies on).
    let record = token_record_of(&w, &p1).expect("token record exists");
    assert_eq!(record.delegate, Some(w.hot.pubkey()));
    assert_eq!(record.delegate_role, Some(TokenDelegateRole::Transfer));
    assert!(record.rule_set_revision.is_some(), "rule-set revision pinned");

    let vault = get_vault(&w);
    assert_eq!(vault.live_buyback_mint, Some(p1.mint));
    assert_eq!(vault.live_buyback_token, Some(p1.token));

    // One live delegate per vault: the second prize must be refused.
    let tx = Transaction::new_signed_with_payer(
        &[approve_buyback_ix(&w, &p2)],
        Some(&w.hot.pubkey()),
        &[&w.hot],
        w.svm.latest_blockhash(),
    );
    assert_fails_with(&mut w, tx, "BuybackSlotOccupied", "second approve_buyback");
}

#[test]
fn approve_buyback_rejected_when_disabled() {
    let mut w = setup(false); // allow_buyback_delegation = false
    let p = mint_prize_to_vault(&mut w);
    let tx = Transaction::new_signed_with_payer(
        &[approve_buyback_ix(&w, &p)],
        Some(&w.hot.pubkey()),
        &[&w.hot],
        w.svm.latest_blockhash(),
    );
    assert_fails_with(
        &mut w,
        tx,
        "BuybackDelegationDisabled",
        "approve_buyback with kill switch on",
    );
}

#[test]
fn approve_buyback_rejects_non_delegate_signer() {
    let mut w = setup(true);
    let p = mint_prize_to_vault(&mut w);
    let attacker = Keypair::new();
    w.svm.airdrop(&attacker.pubkey(), 1_000_000_000).unwrap();

    let mut ix = approve_buyback_ix(&w, &p);
    // Swap the hot delegate for the attacker in the account list (position 2).
    ix.accounts[2].pubkey = attacker.pubkey();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&attacker.pubkey()),
        &[&attacker],
        w.svm.latest_blockhash(),
    );
    // Anchor's has_one = hot_delegate constraint must reject the impostor.
    assert_fails_with(&mut w, tx, "ConstraintHasOne", "attacker approve_buyback");
}

/// The handler-side pin (`require_auth_rules_program`, stable code 6018 in
/// every context): a PRESENT authorization_rules_program slot carrying any
/// program other than mpl-token-auth-rules must be refused before the CPI —
/// this program never forwards a foreign program id.
#[test]
fn approve_buyback_rejects_foreign_auth_rules_program() {
    let mut w = setup(true);
    let p = mint_prize_to_vault(&mut w);

    let mut ix = approve_buyback_ix(&w, &p);
    // Swap the auth-rules program for a random one (slot 9). Any key that is
    // neither auth9Sig... nor this program's id (which would decode as None).
    ix.accounts[9].pubkey = Pubkey::new_unique();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&w.hot.pubkey()),
        &[&w.hot],
        w.svm.latest_blockhash(),
    );
    assert_fails_with(&mut w, tx, "Unauthorized", "foreign auth-rules program");

    // No delegate was granted and the slot stayed free.
    let record = token_record_of(&w, &p).expect("token record exists");
    assert_eq!(record.delegate, None, "no Metaplex delegate granted");
    let vault = get_vault(&w);
    assert_eq!(vault.live_buyback_mint, None);
    assert_eq!(vault.live_buyback_token, None);
}

// ---------------------------------------------------------------------------
// Buyback completion: delegate transfer (CC's leg) + clear_buyback_slot + GC
// ---------------------------------------------------------------------------

#[test]
fn buyback_transfer_by_delegate_then_clear_slot_and_gc() {
    let mut w = setup(true);
    let p = mint_prize_to_vault(&mut w);

    let tx = Transaction::new_signed_with_payer(
        &[approve_buyback_ix(&w, &p)],
        Some(&w.hot.pubkey()),
        &[&w.hot],
        w.svm.latest_blockhash(),
    );
    w.svm.send_transaction(tx).expect("approve_buyback");

    // Simulate CC's buyback transaction with the exact template changes we ask
    // for (README "CC-side asks"): co-signed `:buyback` memo, authority = the transfer
    // delegate (hot key), fee payer = hot key, no closeAccount; the prize goes
    // back to CC's prize wallet.
    let dest_token = ata(&w.cc_operator.pubkey(), &p.mint);
    let dest_token_record = TokenRecord::find_pda(&p.mint, &dest_token).0;

    let hot_before = lamports_of(&w, &w.hot.pubkey());
    let record_rent = lamports_of(&w, &p.token_record);
    // The CC wallet's ATA survives from the premint stage (empty), but its
    // TokenRecord was closed by the `:send` transfer — delta-account both.
    let dest_ata_before = lamports_of(&w, &dest_token);
    let dest_record_before = lamports_of(&w, &dest_token_record);

    let transfer_ix = TransferV1Builder::new()
        .token(p.token)
        .token_owner(w.vault)
        .destination_token(dest_token)
        .destination_owner(w.cc_operator.pubkey())
        .mint(p.mint)
        .metadata(p.metadata)
        .edition(Some(p.edition))
        .token_record(Some(p.token_record))
        .destination_token_record(Some(dest_token_record))
        .authority(w.hot.pubkey())
        .payer(w.hot.pubkey())
        .authorization_rules_program(Some(AUTH_RULES_PROGRAM_ID))
        .authorization_rules(Some(CC_RULE_SET))
        .amount(1)
        .instruction();
    // A CU bump belongs on any pNFT TransferV1 (delegate path + potential
    // destination account creation); the zero-price default keeps the lamport
    // accounting below unchanged (base fee stays 2 signatures).
    let tx = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(300_000),
            cc_memo_ix(&w, "tangem-11111111-2222:buyback"),
            transfer_ix,
        ],
        Some(&w.hot.pubkey()),
        &[&w.hot, &w.cc_operator],
        w.svm.latest_blockhash(),
    );
    w.svm
        .send_transaction(tx)
        .expect("delegate TransferV1 (the CC buyback leg)");
    assert_eq!(spl_amount(&w, &dest_token), 1, "prize left the vault");
    assert_eq!(spl_amount(&w, &p.token), 0);

    // TransferV1 closes the source TokenRecord even in the delegate-authority
    // path — and the rent lands on the TX PAYER (the hot key), so no stale
    // record rent is left behind. Exact accounting: the hot key paid the fee
    // (2 signatures), funded the destination ATA + destination TokenRecord,
    // and got the source record's rent back.
    let record_after = w.svm.get_account(&p.token_record);
    assert!(
        record_after.as_ref().map_or(true, |a| a.lamports == 0),
        "source TokenRecord fully closed, got {record_after:?}"
    );
    let fee = 2 * 5_000;
    let hot_after = lamports_of(&w, &w.hot.pubkey());
    let expected = hot_before - fee
        - (lamports_of(&w, &dest_token) - dest_ata_before)
        - (lamports_of(&w, &dest_token_record) - dest_record_before)
        + record_rent;
    assert_eq!(
        hot_after, expected,
        "source TokenRecord rent refunded to the tx payer (hot key)"
    );

    // The slot still points at the emptied account; closing that ATA now must
    // be refused, or clear_buyback_slot could never present its proof.
    let close_ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::CloseTokenAccountCtx {
            vault: w.vault,
            cold_owner: w.cold.pubkey(),
            token: p.token,
            token_program: TOKEN_PROGRAM_ID,
        }
        .to_account_metas(None),
        data: instruction::CloseTokenAccount {}.data(),
    };
    let tx = Transaction::new_signed_with_payer(
        &[close_ix],
        Some(&w.cold.pubkey()),
        &[&w.cold],
        w.svm.latest_blockhash(),
    );
    assert_fails_with(
        &mut w,
        tx,
        "BuybackStillActive",
        "close_token_account while the slot points at the account",
    );

    // clear_buyback_slot with on-chain proof (the exact delegated account is
    // now empty) frees the slot.
    let ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::ClearBuybackSlot {
            vault: w.vault,
            authority: w.hot.pubkey(),
            nft_token: p.token,
        }
        .to_account_metas(None),
        data: instruction::ClearBuybackSlot {}.data(),
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&w.hot.pubkey()),
        &[&w.hot],
        w.svm.latest_blockhash(),
    );
    w.svm.send_transaction(tx).expect("clear_buyback_slot");
    let vault = get_vault(&w);
    assert_eq!(vault.live_buyback_mint, None);
    assert_eq!(vault.live_buyback_token, None);

    // GC: the cold owner closes the emptied prize ATA and pockets the rent.
    // (Fresh blockhash: the failed early-close above has the same bytes.)
    w.svm.expire_blockhash();
    let cold_before = w.svm.get_account(&w.cold.pubkey()).unwrap().lamports;
    let ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::CloseTokenAccountCtx {
            vault: w.vault,
            cold_owner: w.cold.pubkey(),
            token: p.token,
            token_program: TOKEN_PROGRAM_ID,
        }
        .to_account_metas(None),
        data: instruction::CloseTokenAccount {}.data(),
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&w.cold.pubkey()),
        &[&w.cold],
        w.svm.latest_blockhash(),
    );
    w.svm.send_transaction(tx).expect("close_token_account");
    assert!(w.svm.get_account(&p.token).map_or(true, |a| a.data.is_empty()));
    let cold_after = w.svm.get_account(&w.cold.pubkey()).unwrap().lamports;
    assert!(cold_after > cold_before, "rent refunded to the cold owner");
}

#[test]
fn clear_buyback_slot_rejects_while_delegate_live() {
    let mut w = setup(true);
    let p = mint_prize_to_vault(&mut w);

    let tx = Transaction::new_signed_with_payer(
        &[approve_buyback_ix(&w, &p)],
        Some(&w.hot.pubkey()),
        &[&w.hot],
        w.svm.latest_blockhash(),
    );
    w.svm.send_transaction(tx).expect("approve_buyback");

    // NFT still in the vault, delegate still live — no forged completion.
    let ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::ClearBuybackSlot {
            vault: w.vault,
            authority: w.hot.pubkey(),
            nft_token: p.token,
        }
        .to_account_metas(None),
        data: instruction::ClearBuybackSlot {}.data(),
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&w.hot.pubkey()),
        &[&w.hot],
        w.svm.latest_blockhash(),
    );
    assert_fails_with(&mut w, tx, "BuybackStillActive", "premature clear");
}

#[test]
fn revoke_buyback_clears_delegate_and_frees_slot() {
    let mut w = setup(true);
    let p = mint_prize_to_vault(&mut w);

    let tx = Transaction::new_signed_with_payer(
        &[approve_buyback_ix(&w, &p)],
        Some(&w.hot.pubkey()),
        &[&w.hot],
        w.svm.latest_blockhash(),
    );
    w.svm.send_transaction(tx).expect("approve_buyback");

    // The cold owner reacts (e.g. suspected hot-key compromise) and revokes.
    let ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::RevokeBuyback {
            vault: w.vault,
            authority: w.cold.pubkey(),
            delegate: w.hot.pubkey(),
            nft_mint: p.mint,
            nft_token: p.token,
            metadata: p.metadata,
            edition: p.edition,
            token_record: p.token_record,
            authorization_rules: Some(CC_RULE_SET),
            authorization_rules_program: Some(AUTH_RULES_PROGRAM_ID),
            token_metadata_program: mpl_token_metadata::ID,
            sysvar_instructions: sysvar::instructions::ID,
            token_program: TOKEN_PROGRAM_ID,
            system_program: system_program::ID,
        }
        .to_account_metas(None),
        data: instruction::RevokeBuyback {}.data(),
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&w.cold.pubkey()),
        &[&w.cold],
        w.svm.latest_blockhash(),
    );
    w.svm.send_transaction(tx).expect("revoke_buyback");

    let record = token_record_of(&w, &p).expect("token record exists");
    assert_eq!(record.delegate, None, "TokenRecord delegate cleared");
    let vault = get_vault(&w);
    assert_eq!(vault.live_buyback_mint, None);
    assert_eq!(vault.live_buyback_token, None);

    // The prize is still in the vault and a new approval works again.
    assert_eq!(spl_amount(&w, &p.token), 1);
    // Same instruction as the first approval — advance the blockhash so the
    // tx signature differs (litesvm replay protection).
    w.svm.expire_blockhash();
    let tx = Transaction::new_signed_with_payer(
        &[approve_buyback_ix(&w, &p)],
        Some(&w.hot.pubkey()),
        &[&w.hot],
        w.svm.latest_blockhash(),
    );
    w.svm
        .send_transaction(tx)
        .expect("approve_buyback after revoke");
}

#[test]
fn close_vault_rejected_while_buyback_slot_live() {
    let mut w = setup(true);
    let p = mint_prize_to_vault(&mut w);

    let tx = Transaction::new_signed_with_payer(
        &[approve_buyback_ix(&w, &p)],
        Some(&w.hot.pubkey()),
        &[&w.hot],
        w.svm.latest_blockhash(),
    );
    w.svm.send_transaction(tx).expect("approve_buyback");

    // close_vault + re-init would wipe the slot while the on-chain delegate
    // stayed live — the VaultNotClean gate must refuse.
    let ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::CloseVault {
            vault: w.vault,
            cold_owner: w.cold.pubkey(),
        }
        .to_account_metas(None),
        data: instruction::CloseVault {}.data(),
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&w.cold.pubkey()),
        &[&w.cold],
        w.svm.latest_blockhash(),
    );
    assert_fails_with(&mut w, tx, "VaultNotClean", "close_vault with live slot");
}

// ---------------------------------------------------------------------------
// withdraw_pnft ("rare card → cold storage") + cold-signed tx size
// ---------------------------------------------------------------------------

#[test]
fn withdraw_pnft_cold_only_and_within_packet_budget() {
    let mut w = setup(true);
    let p = mint_prize_to_vault(&mut w);

    let destination_owner = w.cold.pubkey(); // typical: the user's cold wallet
    let destination_token = ata(&destination_owner, &p.mint);
    let destination_token_record = TokenRecord::find_pda(&p.mint, &destination_token).0;

    let withdraw_accounts = |payer: Pubkey| accounts::WithdrawPnft {
        vault: w.vault,
        cold_owner: w.cold.pubkey(),
        payer,
        nft_mint: p.mint,
        nft_token: p.token,
        destination_owner,
        destination_token,
        metadata: p.metadata,
        edition: p.edition,
        token_record: p.token_record,
        destination_token_record,
        authorization_rules: Some(CC_RULE_SET),
        authorization_rules_program: Some(AUTH_RULES_PROGRAM_ID),
        token_metadata_program: mpl_token_metadata::ID,
        sysvar_instructions: sysvar::instructions::ID,
        ata_program: ATA_PROGRAM_ID,
        token_program: TOKEN_PROGRAM_ID,
        system_program: system_program::ID,
    };

    let mut impostor_ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: withdraw_accounts(w.hot.pubkey()).to_account_metas(None),
        data: instruction::WithdrawPnft {}.data(),
    };
    // Impostor in the cold_owner slot. The vault PDA derives from the cold
    // key, so anchor rejects this at the seeds check (before has_one).
    impostor_ix.accounts[1].pubkey = w.hot.pubkey();
    // Production shape: the phone (hot key) pays fees, the card only signs.
    // The pNFT TransferV1 (ATA creation + rule-set validation) does NOT fit
    // the default 200k CU budget, so a compute-budget instruction is REQUIRED
    // — it is part of the cold-signed message and of the size measurement.
    let cu_ix = ComputeBudgetInstruction::set_compute_unit_limit(300_000);
    let ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: withdraw_accounts(w.hot.pubkey()).to_account_metas(None),
        data: instruction::WithdrawPnft {}.data(),
    };

    // The hot key must NOT be able to trigger the withdrawal (cold-only).
    let tx = Transaction::new_signed_with_payer(
        &[impostor_ix],
        Some(&w.hot.pubkey()),
        &[&w.hot],
        w.svm.latest_blockhash(),
    );
    assert_fails_with(&mut w, tx, "ConstraintSeeds", "hot key withdraw_pnft");

    // Negative control for the "compute-budget ix is REQUIRED" claim: the
    // same transaction WITHOUT it must die on the default 200k CU budget.
    let tx = Transaction::new_signed_with_payer(
        &[ix.clone()],
        Some(&w.hot.pubkey()),
        &[&w.hot, &w.cold],
        w.svm.latest_blockhash(),
    );
    match w.svm.send_transaction(tx) {
        Ok(_) => panic!("withdraw_pnft without a compute-budget ix must exhaust 200k CU"),
        Err(f) => {
            let logs = f.meta.logs.join("\n");
            assert!(
                logs.contains("exceeded CUs meter") || logs.contains("Computational budget exceeded"),
                "expected a compute exhaustion, got:\n{logs}\nerr: {:?}",
                f.err
            );
        }
    }
    let tx = Transaction::new_signed_with_payer(
        &[cu_ix, ix],
        Some(&w.hot.pubkey()),
        &[&w.hot, &w.cold],
        w.svm.latest_blockhash(),
    );

    // ---- Cold-signed transaction size measurement (see README) ----
    let message_size = tx.message_data().len();
    // Legacy tx wire size = shortvec(sig count = 2) + 2 * 64 + message.
    let tx_size = 1 + 2 * 64 + message_size;
    println!("withdraw_pnft (18 accounts + compute-budget ix, CC rule set attached):");
    println!("  cold-signed message: {message_size} bytes");
    println!("  serialized transaction: {tx_size} bytes (Solana cap {PACKET_DATA_SIZE})");
    assert!(
        tx_size <= PACKET_DATA_SIZE,
        "withdraw_pnft must fit a single Solana packet"
    );

    w.svm.send_transaction(tx).expect("withdraw_pnft");
    assert_eq!(spl_amount(&w, &destination_token), 1, "prize reached cold wallet");
    assert_eq!(spl_amount(&w, &p.token), 0);

    // TransferV1 closes the source TokenRecord (rent back to the payer =
    // the phone), so nothing vault-side lingers after the withdrawal.
    assert!(token_record_of(&w, &p).is_none());
}

/// Absent-encoding contract for the optional auth-rules slots: a pNFT WITHOUT
/// a rule set is withdrawn with BOTH optional slots encoded as this program's
/// own id (Anchor's `None` wire form) — the slots stay in place, never get
/// dropped, and `require_auth_rules_program` takes its `None` branch.
#[test]
fn withdraw_pnft_without_rule_set_absent_optional_slots() {
    let mut w = setup(true);
    let p = mint_prize_to_vault_with_rule_set(&mut w, None);

    let destination_owner = w.cold.pubkey();
    let destination_token = ata(&destination_owner, &p.mint);
    let destination_token_record = TokenRecord::find_pda(&p.mint, &destination_token).0;

    let ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::WithdrawPnft {
            vault: w.vault,
            cold_owner: w.cold.pubkey(),
            payer: w.payer.pubkey(),
            nft_mint: p.mint,
            nft_token: p.token,
            destination_owner,
            destination_token,
            metadata: p.metadata,
            edition: p.edition,
            token_record: p.token_record,
            destination_token_record,
            authorization_rules: None,
            authorization_rules_program: None,
            token_metadata_program: mpl_token_metadata::ID,
            sysvar_instructions: sysvar::instructions::ID,
            ata_program: ATA_PROGRAM_ID,
            token_program: TOKEN_PROGRAM_ID,
            system_program: system_program::ID,
        }
        .to_account_metas(None),
        data: instruction::WithdrawPnft {}.data(),
    };
    // The account list keeps its full 18-slot shape: absent optionals are
    // FILLED with the program's own id, not dropped (they are not trailing).
    assert_eq!(ix.accounts.len(), 18, "all 18 slots present");
    assert_eq!(
        ix.accounts[11].pubkey, PROGRAM_ID,
        "absent authorization_rules encoded as the program id"
    );
    assert_eq!(
        ix.accounts[12].pubkey, PROGRAM_ID,
        "absent authorization_rules_program encoded as the program id"
    );

    let tx = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(300_000),
            ix,
        ],
        Some(&w.payer.pubkey()),
        &[&w.payer, &w.cold],
        w.svm.latest_blockhash(),
    );
    w.svm
        .send_transaction(tx)
        .expect("withdraw_pnft of a rule-set-less pNFT with absent optional slots");
    assert_eq!(spl_amount(&w, &destination_token), 1, "prize reached the destination");
    assert_eq!(spl_amount(&w, &p.token), 0);
}

// ---------------------------------------------------------------------------
// Slot integrity, authorization, and reactive controls
// ---------------------------------------------------------------------------

#[test]
fn pause_blocks_approve_buyback() {
    let mut w = setup(true);
    let p = mint_prize_to_vault(&mut w);
    write_config(&mut w.svm, true, true); // paused = true
    let tx = Transaction::new_signed_with_payer(
        &[approve_buyback_ix(&w, &p)],
        Some(&w.hot.pubkey()),
        &[&w.hot],
        w.svm.latest_blockhash(),
    );
    assert_fails_with(&mut w, tx, "Paused", "approve_buyback while paused");
}

#[test]
fn clear_buyback_slot_rejects_decoy_and_requires_live_slot() {
    let mut w = setup(true);
    let p = mint_prize_to_vault(&mut w);

    let vault = w.vault;
    let clear_ix = move |token: Pubkey, authority: Pubkey| Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::ClearBuybackSlot {
            vault,
            authority,
            nft_token: token,
        }
        .to_account_metas(None),
        data: instruction::ClearBuybackSlot {}.data(),
    };

    // No live buyback yet — nothing to clear.
    let tx = Transaction::new_signed_with_payer(
        &[clear_ix(p.token, w.hot.pubkey())],
        Some(&w.hot.pubkey()),
        &[&w.hot],
        w.svm.latest_blockhash(),
    );
    assert_fails_with(&mut w, tx, "NoLiveBuyback", "clear with empty slot");

    let tx = Transaction::new_signed_with_payer(
        &[approve_buyback_ix(&w, &p)],
        Some(&w.hot.pubkey()),
        &[&w.hot],
        w.svm.latest_blockhash(),
    );
    w.svm.send_transaction(tx).expect("approve_buyback");

    // The decoy attack the exact-token check exists for: anyone can create a
    // fresh vault-owned token account of the same mint (amount 0, no
    // delegate) WITHOUT the vault signing. Presenting it must not free the
    // slot while the real Metaplex delegate is live.
    let decoy = create_decoy_vault_token_account(&mut w, &p.mint);
    let tx = Transaction::new_signed_with_payer(
        &[clear_ix(decoy, w.hot.pubkey())],
        Some(&w.hot.pubkey()),
        &[&w.hot],
        w.svm.latest_blockhash(),
    );
    assert_fails_with(&mut w, tx, "WrongBuybackToken", "clear with a decoy account");
    assert!(get_vault(&w).live_buyback_mint.is_some(), "slot stays occupied");
}

#[test]
fn revoke_and_clear_reject_unauthorized_signer() {
    let mut w = setup(true);
    let p = mint_prize_to_vault(&mut w);
    let attacker = Keypair::new();
    w.svm.airdrop(&attacker.pubkey(), 1_000_000_000).unwrap();

    let tx = Transaction::new_signed_with_payer(
        &[approve_buyback_ix(&w, &p)],
        Some(&w.hot.pubkey()),
        &[&w.hot],
        w.svm.latest_blockhash(),
    );
    w.svm.send_transaction(tx).expect("approve_buyback");

    // Neither instruction has an anchor has_one — authorization lives in the
    // handlers (cold OR hot). A third-party signer must be rejected by both.
    let ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::ClearBuybackSlot {
            vault: w.vault,
            authority: attacker.pubkey(),
            nft_token: p.token,
        }
        .to_account_metas(None),
        data: instruction::ClearBuybackSlot {}.data(),
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&attacker.pubkey()),
        &[&attacker],
        w.svm.latest_blockhash(),
    );
    assert_fails_with(&mut w, tx, "Unauthorized", "attacker clear_buyback_slot");

    let ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::RevokeBuyback {
            vault: w.vault,
            authority: attacker.pubkey(),
            delegate: w.hot.pubkey(),
            nft_mint: p.mint,
            nft_token: p.token,
            metadata: p.metadata,
            edition: p.edition,
            token_record: p.token_record,
            authorization_rules: Some(CC_RULE_SET),
            authorization_rules_program: Some(AUTH_RULES_PROGRAM_ID),
            token_metadata_program: mpl_token_metadata::ID,
            sysvar_instructions: sysvar::instructions::ID,
            token_program: TOKEN_PROGRAM_ID,
            system_program: system_program::ID,
        }
        .to_account_metas(None),
        data: instruction::RevokeBuyback {}.data(),
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&attacker.pubkey()),
        &[&attacker],
        w.svm.latest_blockhash(),
    );
    assert_fails_with(&mut w, tx, "Unauthorized", "attacker revoke_buyback");
    assert!(get_vault(&w).live_buyback_mint.is_some(), "slot untouched");
}

/// The documented reactive control: the cold key can rescue a prize even
/// while a (possibly hostile) buyback delegate is live — the owner-path
/// TransferV1 overrides and consumes the delegate — and the slot is then
/// freed with the emptied account as proof.
#[test]
fn cold_rescue_withdraws_delegated_prize_and_frees_slot() {
    let mut w = setup(true);
    let p = mint_prize_to_vault(&mut w);

    let tx = Transaction::new_signed_with_payer(
        &[approve_buyback_ix(&w, &p)],
        Some(&w.hot.pubkey()),
        &[&w.hot],
        w.svm.latest_blockhash(),
    );
    w.svm.send_transaction(tx).expect("approve_buyback");

    let destination_owner = w.cold.pubkey();
    let destination_token = ata(&destination_owner, &p.mint);
    let destination_token_record = TokenRecord::find_pda(&p.mint, &destination_token).0;
    let ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::WithdrawPnft {
            vault: w.vault,
            cold_owner: w.cold.pubkey(),
            payer: w.payer.pubkey(),
            nft_mint: p.mint,
            nft_token: p.token,
            destination_owner,
            destination_token,
            metadata: p.metadata,
            edition: p.edition,
            token_record: p.token_record,
            destination_token_record,
            authorization_rules: Some(CC_RULE_SET),
            authorization_rules_program: Some(AUTH_RULES_PROGRAM_ID),
            token_metadata_program: mpl_token_metadata::ID,
            sysvar_instructions: sysvar::instructions::ID,
            ata_program: ATA_PROGRAM_ID,
            token_program: TOKEN_PROGRAM_ID,
            system_program: system_program::ID,
        }
        .to_account_metas(None),
        data: instruction::WithdrawPnft {}.data(),
    };
    let tx = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(300_000),
            ix,
        ],
        Some(&w.payer.pubkey()),
        &[&w.payer, &w.cold],
        w.svm.latest_blockhash(),
    );
    w.svm
        .send_transaction(tx)
        .expect("cold withdraw_pnft while a buyback delegate is live");
    assert_eq!(spl_amount(&w, &destination_token), 1, "prize rescued to cold");

    // Free the slot with the emptied exact account as proof.
    let ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::ClearBuybackSlot {
            vault: w.vault,
            authority: w.cold.pubkey(),
            nft_token: p.token,
        }
        .to_account_metas(None),
        data: instruction::ClearBuybackSlot {}.data(),
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&w.cold.pubkey()),
        &[&w.cold],
        w.svm.latest_blockhash(),
    );
    w.svm.send_transaction(tx).expect("clear_buyback_slot after rescue");
    assert_eq!(get_vault(&w).live_buyback_mint, None);
}

/// Documents the residual risk the README warns about: rotating the hot key
/// via update_vault does NOT touch already-granted Metaplex delegates — the
/// OLD hot key can still move the delegated pNFT out entirely outside this
/// program. Pair every rotation with revoke_buyback.
#[test]
fn hot_rotation_leaves_onchain_delegate_live() {
    let mut w = setup(true);
    let p = mint_prize_to_vault(&mut w);

    let tx = Transaction::new_signed_with_payer(
        &[approve_buyback_ix(&w, &p)],
        Some(&w.hot.pubkey()),
        &[&w.hot],
        w.svm.latest_blockhash(),
    );
    w.svm.send_transaction(tx).expect("approve_buyback");

    // Cold rotates the hot delegate away from the (compromised) key.
    let new_hot = Keypair::new();
    let ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::ColdAuthority {
            vault: w.vault,
            cold_owner: w.cold.pubkey(),
        }
        .to_account_metas(None),
        data: instruction::UpdateVault {
            hot_delegate: Some(new_hot.pubkey()),
            per_spin_cap: None,
            daily_cap: None,
        }
        .data(),
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&w.cold.pubkey()),
        &[&w.cold],
        w.svm.latest_blockhash(),
    );
    w.svm.send_transaction(tx).expect("update_vault rotation");

    // The OLD hot key is no longer the vault's delegate program-side, but the
    // Metaplex TokenRecord delegate is still live: it can still drain the NFT.
    let attacker_wallet = Keypair::new();
    w.svm.airdrop(&attacker_wallet.pubkey(), 1_000_000_000).unwrap();
    let dest_token = ata(&attacker_wallet.pubkey(), &p.mint);
    let dest_token_record = TokenRecord::find_pda(&p.mint, &dest_token).0;
    let transfer_ix = TransferV1Builder::new()
        .token(p.token)
        .token_owner(w.vault)
        .destination_token(dest_token)
        .destination_owner(attacker_wallet.pubkey())
        .mint(p.mint)
        .metadata(p.metadata)
        .edition(Some(p.edition))
        .token_record(Some(p.token_record))
        .destination_token_record(Some(dest_token_record))
        .authority(w.hot.pubkey())
        .payer(w.hot.pubkey())
        .authorization_rules_program(Some(AUTH_RULES_PROGRAM_ID))
        .authorization_rules(Some(CC_RULE_SET))
        .amount(1)
        .instruction();
    // A pNFT TransferV1 that lazily creates the destination ATA + TokenRecord
    // sits right at the 200k default CU limit, so it needs a compute-budget
    // bump (same as withdraw_pnft / any real pNFT move); without it the tx
    // flakes on the CU meter depending on the random destination's PDA bumps.
    let tx = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(300_000),
            transfer_ix,
        ],
        Some(&w.hot.pubkey()),
        &[&w.hot],
        w.svm.latest_blockhash(),
    );
    w.svm
        .send_transaction(tx)
        .expect("rotation alone must NOT stop the old delegate (documented residual risk)");
    assert_eq!(spl_amount(&w, &dest_token), 1, "old hot key drained the NFT");

    // Recovery: the NEW hot key frees the slot with the emptied account.
    let ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::ClearBuybackSlot {
            vault: w.vault,
            authority: new_hot.pubkey(),
            nft_token: p.token,
        }
        .to_account_metas(None),
        data: instruction::ClearBuybackSlot {}.data(),
    };
    w.svm.airdrop(&new_hot.pubkey(), 1_000_000_000).unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&new_hot.pubkey()),
        &[&new_hot],
        w.svm.latest_blockhash(),
    );
    w.svm.send_transaction(tx).expect("new hot clears the slot");
    assert_eq!(get_vault(&w).live_buyback_mint, None);
}

/// The premise the whole delegation design rests on: pNFT token accounts are
/// permanently frozen, so plain SPL transfer paths fail even for the rightful
/// on-curve owner (SPL approve/transfer do not work on pNFTs).
#[test]
fn prize_spl_transfer_rejected_frozen() {
    let mut w = setup(true);
    let p = mint_prize_to_vault(&mut w);

    // Owner-side check happens at the CC wallet stage in mint_prize_to_vault
    // (the vault ATA assert there); here try the vault-held account: a plain
    // SPL TransferChecked to a same-mint decoy, "signed" by the only party
    // that could ever try — it must die on the frozen state, proving the SPL
    // path is closed regardless of authority.
    let decoy = create_decoy_vault_token_account(&mut w, &p.mint);
    // spl-token TransferChecked: tag 12 + amount u64 + decimals u8.
    let mut data = vec![12u8];
    data.extend_from_slice(&1u64.to_le_bytes());
    data.push(0);
    let ix = Instruction {
        program_id: TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(p.token, false),
            AccountMeta::new_readonly(p.mint, false),
            AccountMeta::new(decoy, false),
            AccountMeta::new_readonly(w.hot.pubkey(), true),
        ],
        data,
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&w.hot.pubkey()),
        &[&w.hot],
        w.svm.latest_blockhash(),
    );
    match w.svm.send_transaction(tx) {
        Ok(_) => panic!("plain SPL transfer of a pNFT must fail"),
        Err(f) => {
            let logs = f.meta.logs.join("\n");
            assert!(
                logs.contains("frozen") || logs.contains("owner does not match"),
                "expected the SPL path to be closed, got:\n{logs}"
            );
        }
    }
    assert_eq!(spl_amount(&w, &p.token), 1, "prize still in the vault");
}

// ---------------------------------------------------------------------------
// buyback_pnft — the atomic in-program buyback (no delegate involved)
// ---------------------------------------------------------------------------

/// SPL mint with 6 decimals (the USDC shape).
fn create_spl_mint(w: &mut World) -> Pubkey {
    let mint_kp = Keypair::new();
    let rent = w.svm.minimum_balance_for_rent_exemption(82);
    let create = system_instruction::create_account(
        &w.payer.pubkey(),
        &mint_kp.pubkey(),
        rent,
        82,
        &TOKEN_PROGRAM_ID,
    );
    let mut data = vec![20u8, 6]; // InitializeMint2, 6 decimals
    data.extend_from_slice(w.payer.pubkey().as_ref());
    data.push(0);
    let init = Instruction {
        program_id: TOKEN_PROGRAM_ID,
        accounts: vec![AccountMeta::new(mint_kp.pubkey(), false)],
        data,
    };
    let payer = w.payer.insecure_clone();
    let tx = Transaction::new_signed_with_payer(
        &[create, init],
        Some(&payer.pubkey()),
        &[&payer, &mint_kp],
        w.svm.latest_blockhash(),
    );
    w.svm.send_transaction(tx).expect("create usdc mint");
    mint_kp.pubkey()
}

fn create_spl_ata(w: &mut World, owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    let addr = ata(owner, mint);
    let ix = Instruction {
        program_id: ATA_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(w.payer.pubkey(), true),
            AccountMeta::new(addr, false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        ],
        data: vec![1], // CreateIdempotent
    };
    let payer = w.payer.insecure_clone();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[&payer],
        w.svm.latest_blockhash(),
    );
    w.svm.send_transaction(tx).expect("create ata");
    addr
}

fn mint_spl_to(w: &mut World, mint: &Pubkey, dest: &Pubkey, amount: u64) {
    let mut data = vec![7u8];
    data.extend_from_slice(&amount.to_le_bytes());
    let ix = Instruction {
        program_id: TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*mint, false),
            AccountMeta::new(*dest, false),
            AccountMeta::new_readonly(w.payer.pubkey(), true),
        ],
        data,
    };
    let payer = w.payer.insecure_clone();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[&payer],
        w.svm.latest_blockhash(),
    );
    w.svm.send_transaction(tx).expect("mint_to");
}

/// Rewrites the config so the buyback pins point at REAL accounts: a live
/// USDC mint and cc_operator as the gacha (CC operator) wallet.
fn patch_config(w: &mut World, usdc_mint: Pubkey, paused: bool) {
    let (config_pda, bump) = Pubkey::find_program_address(&[CONFIG_SEED], &PROGRAM_ID);
    let cfg = Config {
        admin: Pubkey::new_unique(),
        pending_admin: Pubkey::default(),
        usdc_mint,
        rent_destination: w.cc_operator.pubkey(),
        gacha_usdc_account: Pubkey::new_unique(),
        fee_usdc_account: Pubkey::new_unique(),
        fee_bps: 0,
        paused,
        _reserved: false,
        allow_buyback_delegation: true,
        bump,
        _padding: [0u8; 128],
    };
    let mut data = Vec::new();
    cfg.try_serialize(&mut data).unwrap();
    let lamports = w.svm.minimum_balance_for_rent_exemption(data.len());
    w.svm
        .set_account(
            config_pda,
            solana_sdk::account::Account {
                lamports,
                data,
                owner: PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
}

/// USDC world for the buyback tests: mint + vault/CC ATAs, CC funded.
fn install_usdc(w: &mut World) -> (Pubkey, Pubkey, Pubkey) {
    let usdc_mint = create_spl_mint(w);
    patch_config(w, usdc_mint, false);
    let vault = w.vault;
    let cc = w.cc_operator.pubkey();
    let vault_usdc = create_spl_ata(w, &vault, &usdc_mint);
    let cc_usdc = create_spl_ata(w, &cc, &usdc_mint);
    mint_spl_to(w, &usdc_mint, &cc_usdc, 1_000_000_000); // 1000 USDC
    (usdc_mint, vault_usdc, cc_usdc)
}

fn buyback_pnft_ix(
    w: &World,
    p: &Prize,
    destination_owner: Pubkey,
    usdc_mint: Pubkey,
    cc_usdc: Pubkey,
    price: u64,
) -> Instruction {
    let dest_token = ata(&destination_owner, &p.mint);
    Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::BuybackPnft {
            config: w.config,
            vault: w.vault,
            hot_delegate: w.hot.pubkey(),
            cc_authority: w.cc_operator.pubkey(),
            destination_owner,
            usdc_mint,
            cc_usdc,
            vault_usdc: ata(&w.vault, &usdc_mint),
            nft_mint: p.mint,
            nft_token: p.token,
            destination_token: dest_token,
            metadata: p.metadata,
            edition: p.edition,
            token_record: p.token_record,
            destination_token_record: TokenRecord::find_pda(&p.mint, &dest_token).0,
            authorization_rules: Some(CC_RULE_SET),
            authorization_rules_program: Some(AUTH_RULES_PROGRAM_ID),
            token_metadata_program: mpl_token_metadata::ID,
            sysvar_instructions: sysvar::instructions::ID,
            ata_program: ATA_PROGRAM_ID,
            token_program: TOKEN_PROGRAM_ID,
            system_program: system_program::ID,
        }
        .to_account_metas(None),
        data: instruction::BuybackPnft {
            price,
            memo: CC_BUYBACK_MEMO.to_string(),
        }
        .data(),
    }
}

/// Production shape of CC's buyback transaction per the builder contract:
/// idempotent vault-USDC ATA creation (paid by CC; a no-op when the ATA
/// exists) + compute budget + top-level memo + the buyback instruction,
/// fee payer CC, signed by CC and the hot key. The ATA leg derives the
/// CANONICAL vault-USDC ATA itself (vault at index 1, usdc_mint at index 5)
/// exactly like CC's builder would — deliberately NOT read from the buyback
/// instruction's vault_usdc slot, so a test that plants a decoy there still
/// exercises the program's own ATA pin rather than the ATA program's.
fn signed_buyback_tx(w: &World, ix: Instruction) -> Transaction {
    let cc = w.cc_operator.insecure_clone();
    let hot = w.hot.insecure_clone();
    let (canonical_vault_usdc, _) = Pubkey::find_program_address(
        &[
            ix.accounts[1].pubkey.as_ref(),
            TOKEN_PROGRAM_ID.as_ref(),
            ix.accounts[5].pubkey.as_ref(),
        ],
        &ATA_PROGRAM_ID,
    );
    let create_vault_usdc = Instruction {
        program_id: ATA_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(cc.pubkey(), true),
            AccountMeta::new(canonical_vault_usdc, false),
            AccountMeta::new_readonly(ix.accounts[1].pubkey, false),
            AccountMeta::new_readonly(ix.accounts[5].pubkey, false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        ],
        data: vec![1], // CreateIdempotent
    };
    Transaction::new_signed_with_payer(
        &[
            create_vault_usdc,
            ComputeBudgetInstruction::set_compute_unit_limit(400_000),
            memo_ix(CC_BUYBACK_MEMO),
            ix,
        ],
        Some(&cc.pubkey()),
        &[&cc, &hot],
        w.svm.latest_blockhash(),
    )
}

/// Happy path: CC pays, the vault gets the USDC, CC gets the NFT, the
/// emptied prize ATA is closed in the same instruction.
#[test]
fn buyback_pnft_atomic_swap_and_gc() {
    let mut w = setup(true);
    let p = mint_prize_to_vault(&mut w);
    let (usdc_mint, vault_usdc, cc_usdc) = install_usdc(&mut w);
    let price = 100_000_000; // 100 USDC

    // The closed prize ATA's lamports must go to CC, who fronted that rent —
    // not to the vault and not to the cold owner. Without this, a regression of
    // the close destination to either of those two would still pass every other
    // assertion here.
    let ata_lamports = lamports_of(&w, &p.token);
    assert!(ata_lamports > 0, "prize ATA is rent-funded before the buyback");
    let vault_lamports_before = lamports_of(&w, &w.vault);
    let cold_lamports_before = lamports_of(&w, &w.cold.pubkey());

    // CC's prizes live on rotating prize wallets — the NFT destination is a
    // different key than the signing operator wallet, and that must work.
    let prize_wallet = Pubkey::new_unique();
    let ix = buyback_pnft_ix(&w, &p, prize_wallet, usdc_mint, cc_usdc, price);
    let tx = signed_buyback_tx(&w, ix);

    // ---- Buyback transaction size measurement (CC builds this tx) ----
    // Full production shape per the builder contract: idempotent vault-USDC
    // ATA creation + compute-budget 400k + top-level memo of realistic
    // dev-slug length + the 22-account buyback_pnft instruction, two
    // signatures (CC + hot).
    let message_size = tx.message_data().len();
    // Legacy tx wire size = shortvec(sig count = 2) + 2 * 64 + message.
    let tx_size = 1 + 2 * 64 + message_size;
    println!("buyback_pnft (create-idempotent + compute-budget + top-level memo + 22 accounts):");
    println!("  message: {message_size} bytes");
    println!("  serialized transaction: {tx_size} bytes (Solana cap {PACKET_DATA_SIZE})");
    assert!(
        tx_size <= PACKET_DATA_SIZE,
        "buyback_pnft must fit a single Solana packet ({tx_size} > {PACKET_DATA_SIZE})"
    );

    let meta = w.svm.send_transaction(tx).expect("buyback_pnft");

    // Both things CC's matcher greps the logs for must actually be there:
    // the Memo program echoing the memo text, and the BuybackExecuted event.
    let logs = &meta.logs;
    assert!(
        logs.iter().any(|l| l.contains(&format!("Program {MEMO_PROGRAM_ID} invoke [1]"))),
        "top-level Memo instruction executed, got:\n{}",
        logs.join("\n")
    );
    assert!(
        logs.iter().any(|l| l.contains("Memo") && l.contains(CC_BUYBACK_MEMO)),
        "Memo program logs the memo text, got:\n{}",
        logs.join("\n")
    );
    let ev: common::BuybackExecuted =
        common::decode_event(logs).expect("BuybackExecuted was emitted and decodes");
    assert_eq!(ev.vault, w.vault);
    assert_eq!(ev.mint, p.mint);
    assert_eq!(ev.price, price);
    assert_eq!(ev.memo, CC_BUYBACK_MEMO);

    assert_eq!(
        lamports_of(&w, &w.vault),
        vault_lamports_before,
        "prize-ATA lamports must not be routed to the vault"
    );
    assert_eq!(
        lamports_of(&w, &w.cold.pubkey()),
        cold_lamports_before,
        "prize-ATA lamports must not be routed to the cold owner"
    );

    assert_eq!(spl_amount(&w, &vault_usdc), price, "refund landed in the vault");
    assert_eq!(
        spl_amount(&w, &cc_usdc),
        1_000_000_000 - price,
        "CC paid the price"
    );
    let dest_nft = ata(&prize_wallet, &p.mint);
    assert_eq!(spl_amount(&w, &dest_nft), 1, "NFT landed on CC's prize wallet");
    let op_nft = w
        .svm
        .get_account(&ata(&w.cc_operator.pubkey(), &p.mint))
        .map_or(0, |a| u64::from_le_bytes(a.data[64..72].try_into().unwrap()));
    assert_eq!(op_nft, 0, "the signing operator wallet did not receive the NFT");
    assert!(
        w.svm.get_account(&p.token).map_or(true, |a| a.data.is_empty()),
        "emptied prize ATA was closed in the same instruction"
    );
    let v = get_vault(&w);
    assert!(v.live_buyback_mint.is_none() && v.live_buyback_token.is_none());
}

/// A buyback of an NFT that still carries a live approve_buyback delegate
/// consumes the delegate (owner-path transfer) and frees the slot.
#[test]
fn buyback_pnft_consumes_stale_delegate_and_frees_slot() {
    let mut w = setup(true);
    let p = mint_prize_to_vault(&mut w);
    let (usdc_mint, vault_usdc, cc_usdc) = install_usdc(&mut w);

    let hot = w.hot.insecure_clone();
    let tx = Transaction::new_signed_with_payer(
        &[approve_buyback_ix(&w, &p)],
        Some(&hot.pubkey()),
        &[&hot],
        w.svm.latest_blockhash(),
    );
    w.svm.send_transaction(tx).expect("approve_buyback");
    assert_eq!(get_vault(&w).live_buyback_token, Some(p.token));

    let price = 50_000_000;
    let ix = buyback_pnft_ix(&w, &p, w.cc_operator.pubkey(), usdc_mint, cc_usdc, price);
    let tx = signed_buyback_tx(&w, ix);
    w.svm
        .send_transaction(tx)
        .expect("buyback_pnft over a live delegate (owner path)");

    assert_eq!(spl_amount(&w, &vault_usdc), price);
    assert_eq!(spl_amount(&w, &ata(&w.cc_operator.pubkey(), &p.mint)), 1);
    let v = get_vault(&w);
    assert!(
        v.live_buyback_mint.is_none() && v.live_buyback_token.is_none(),
        "stale slot freed by the buyback"
    );
}

/// Only the config-pinned CC wallet can execute a buyback — a stranger
/// paying the same price is refused, so a compromised hot key alone cannot
/// move an NFT through this path.
#[test]
fn buyback_pnft_rejects_non_cc_signer() {
    let mut w = setup(true);
    let p = mint_prize_to_vault(&mut w);
    let (usdc_mint, _, _) = install_usdc(&mut w);

    let stranger = Keypair::new();
    w.svm.airdrop(&stranger.pubkey(), 10_000_000_000).unwrap();
    let stranger_usdc = create_spl_ata(&mut w, &stranger.pubkey(), &usdc_mint);
    mint_spl_to(&mut w, &usdc_mint, &stranger_usdc, 500_000_000);

    // The stranger signs, pays and names itself the destination — the builder
    // derives destination_token/token_record from the destination arg, so only
    // the cc_authority slot (index 3) needs patching.
    let mut ix = buyback_pnft_ix(&w, &p, stranger.pubkey(), usdc_mint, stranger_usdc, 100_000_000);
    ix.accounts[3].pubkey = stranger.pubkey();

    let hot = w.hot.insecure_clone();
    let tx = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(400_000),
            ix,
        ],
        Some(&stranger.pubkey()),
        &[&stranger, &hot],
        w.svm.latest_blockhash(),
    );
    assert_fails_with(&mut w, tx, "ConstraintAddress", "non-CC buyback signer");
    assert_eq!(spl_amount(&w, &p.token), 1, "prize stays in the vault");
}

/// Without the hot key's consent co-signature the buyback is refused — CC
/// alone cannot pull an NFT out of the vault.
#[test]
fn buyback_pnft_rejects_without_hot_consent() {
    let mut w = setup(true);
    let p = mint_prize_to_vault(&mut w);
    let (usdc_mint, _, cc_usdc) = install_usdc(&mut w);

    let stranger = Keypair::new();
    w.svm.airdrop(&stranger.pubkey(), 1_000_000_000).unwrap();
    let mut ix = buyback_pnft_ix(&w, &p, w.cc_operator.pubkey(), usdc_mint, cc_usdc, 100_000_000);
    ix.accounts[2].pubkey = stranger.pubkey(); // impostor in the hot slot

    let cc = w.cc_operator.insecure_clone();
    let tx = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(400_000),
            ix,
        ],
        Some(&cc.pubkey()),
        &[&cc, &stranger],
        w.svm.latest_blockhash(),
    );
    assert_fails_with(&mut w, tx, "ConstraintHasOne", "buyback without hot consent");
    assert_eq!(spl_amount(&w, &p.token), 1, "prize stays in the vault");
}

/// Paused program blocks buybacks; a zero price is refused outright.
#[test]
fn buyback_pnft_rejects_paused_and_zero_price() {
    let mut w = setup(true);
    let p = mint_prize_to_vault(&mut w);
    let (usdc_mint, _, cc_usdc) = install_usdc(&mut w);

    let ix = buyback_pnft_ix(&w, &p, w.cc_operator.pubkey(), usdc_mint, cc_usdc, 0);
    let tx = signed_buyback_tx(&w, ix);
    assert_fails_with(&mut w, tx, "ZeroAmount", "zero-price buyback");

    patch_config(&mut w, usdc_mint, true);
    let ix = buyback_pnft_ix(&w, &p, w.cc_operator.pubkey(), usdc_mint, cc_usdc, 100_000_000);
    let tx = signed_buyback_tx(&w, ix);
    assert_fails_with(&mut w, tx, "Paused", "buyback while paused");
    assert_eq!(spl_amount(&w, &p.token), 1, "prize stays in the vault");
}

/// THE core economic guarantee, exercised: if CC's payment leg fails
/// (price exceeds CC's balance), the WHOLE instruction reverts and the
/// NFT never leaves the vault.
#[test]
fn buyback_pnft_reverts_atomically_when_payment_fails() {
    let mut w = setup(true);
    let p = mint_prize_to_vault(&mut w);
    let (usdc_mint, vault_usdc, cc_usdc) = install_usdc(&mut w);

    // CC holds 1000 USDC; demand more than it can pay.
    let ix = buyback_pnft_ix(&w, &p, w.cc_operator.pubkey(), usdc_mint, cc_usdc, 2_000_000_000);
    let tx = signed_buyback_tx(&w, ix);
    assert_fails_with(&mut w, tx, "insufficient funds", "underfunded buyback");

    assert_eq!(spl_amount(&w, &p.token), 1, "NFT never left the vault");
    assert_eq!(spl_amount(&w, &vault_usdc), 0, "no partial refund recorded");
    assert!(
        w.svm.get_account(&p.token).is_some(),
        "prize ATA not closed by the failed attempt"
    );
}

/// The refund cannot be steered away from the vault's canonical USDC ATA:
/// a plain vault-owned USDC account in the vault_usdc slot is refused.
#[test]
fn buyback_pnft_rejects_non_canonical_vault_usdc() {
    let mut w = setup(true);
    let p = mint_prize_to_vault(&mut w);
    let (usdc_mint, _, cc_usdc) = install_usdc(&mut w);
    let decoy = create_decoy_vault_token_account(&mut w, &usdc_mint);

    let mut ix = buyback_pnft_ix(&w, &p, w.cc_operator.pubkey(), usdc_mint, cc_usdc, 100_000_000);
    ix.accounts[7].pubkey = decoy; // vault_usdc slot (after cc_usdc)

    let tx = signed_buyback_tx(&w, ix);
    assert_fails_with(
        &mut w,
        tx,
        "ConstraintAssociated",
        "non-canonical vault USDC account",
    );
    assert_eq!(spl_amount(&w, &p.token), 1, "prize stays in the vault");
}

// ===========================================================================
// buyback_pnft_v2 — CC signs a quote, not this transaction
// ===========================================================================
//
// 24 accounts, which does not fit a legacy packet, so the client builds a v0
// transaction against CC's frozen address lookup table. Both facts are pinned
// below: the legacy form must be over budget and the v0 form must be under it,
// so the ALT dependency can never regress silently in either direction.

/// The CC side of a v2 buyback: cc_buyback's program, its policy, and a USDC
/// treasury delegated to its policy PDA.
///
/// Reconstructed helper — the wire-format declarations it used to sit beside now
/// live in common/mod.rs, shared with cc_buyback_v2.rs.
struct V2World {
    destination: Pubkey,
    usdc_mint: Pubkey,
    cc_usdc: Pubkey,
    vault_usdc: Pubkey,
    cc_policy: Pubkey,
    quote_signer: Keypair,
}

fn setup_v2(w: &mut World) -> V2World {
    w.svm
        .add_program_from_file(CC_BUYBACK_ID, fixture("cc_buyback.so"))
        .expect("cc_buyback.so — copy it from gachamachine/solana/target/deploy");

    let (usdc_mint, vault_usdc, cc_usdc) = install_usdc(w);
    let (cc_policy, cc_bump) = cc_policy_pda();
    let quote_signer = Keypair::new();
    let destination = Pubkey::new_unique();

    // cc_buyback pulls as spl-token DELEGATE; the treasury's owner stays CC's
    // wallet, which is what keeps the Alchemy webhook's payer predicate true.
    delegate_spl(w, &cc_usdc, &cc_policy, 500_000_000);

    plant_cc_policy(
        &mut w.svm,
        cc_policy,
        cc_bump,
        quote_signer.pubkey(),
        &[destination],
        &[CcLane { mint: usdc_mint, treasury: cc_usdc }],
        w.cc_operator.pubkey(),
    );

    // cc_buyback fronts the quote marker's rent from its own PDA, so the phone
    // never pays for CC's bookkeeping. Unfunded, every buyback fails.
    w.svm.airdrop(&cc_rent_vault_pda().0, 1_000_000_000).unwrap();

    V2World { destination, usdc_mint, cc_usdc, vault_usdc, cc_policy, quote_signer }
}

/// spl-token `approve`, written straight into the account: the delegate sits at
/// [72..108] behind a 4-byte COption tag, delegated_amount at [121..129].
fn delegate_spl(w: &mut World, token: &Pubkey, delegate: &Pubkey, amount: u64) {
    let mut acct = w.svm.get_account(token).expect("token account");
    acct.data[72..76].copy_from_slice(&1u32.to_le_bytes());
    acct.data[76..108].copy_from_slice(delegate.as_ref());
    acct.data[121..129].copy_from_slice(&amount.to_le_bytes());
    w.svm.set_account(*token, acct).unwrap();
}

fn buyback_pnft_v2_ix(w: &World, v: &V2World, p: &Prize, price: u64, quote_id: u64, expires_at: i64) -> Instruction {
    // Keyed on the digest, so the caller recomputes it — what a real integrator does.
    let digest = cc_quote_digest(&w.vault, &p.mint, &v.destination, &v.usdc_mint, price, expires_at, quote_id, CC_BUYBACK_MEMO);
    Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::BuybackPnftV2 {
            cc_quote_marker: cc_quote_marker_pda(&digest),
            cc_rent_vault: cc_rent_vault_pda().0,
            cc_memo_program: tangem_gacha_vault::CC_MEMO_ID,
            config: Pubkey::find_program_address(&[CONFIG_SEED], &PROGRAM_ID).0,
            vault: w.vault,
            hot_delegate: w.hot.pubkey(),
            cc_authority: w.cc_operator.pubkey(),
            destination_owner: v.destination,
            usdc_mint: v.usdc_mint,
            cc_usdc: v.cc_usdc,
            vault_usdc: v.vault_usdc,
            nft_mint: p.mint,
            nft_token: p.token,
            destination_token: ata(&v.destination, &p.mint),
            metadata: p.metadata,
            edition: p.edition,
            token_record: p.token_record,
            destination_token_record: TokenRecord::find_pda(&p.mint, &ata(&v.destination, &p.mint)).0,
            authorization_rules: Some(CC_RULE_SET),
            authorization_rules_program: Some(AUTH_RULES_PROGRAM_ID),
            token_metadata_program: mpl_token_metadata::ID,
            sysvar_instructions: solana_sdk::sysvar::instructions::ID,
            ata_program: ATA_PROGRAM_ID,
            cc_program: CC_BUYBACK_ID,
            cc_policy: v.cc_policy,
            token_program: TOKEN_PROGRAM_ID,
            system_program: system_program::ID,
        }
        .to_account_metas(None),
        data: instruction::BuybackPnftV2 { price, quote_id, expires_at, memo: CC_BUYBACK_MEMO.to_string() }.data(),
    }
}

/// The address lookup table CC publishes and freezes. Every entry is a constant
/// — programs, sysvars, and CC's own accounts — so the table never needs
/// extending for a new prize or a new user.
fn plant_alt(w: &mut World, v: &V2World, addresses: Vec<Pubkey>) -> AddressLookupTableAccount {
    let key = Pubkey::new_unique();
    let _ = v;
    // LOOKUP_TABLE_META_SIZE is a fixed 56 bytes: bincode enum tag (4) +
    // deactivation_slot (8) + last_extended_slot (8) + start_index (1) +
    // Option<Pubkey> authority (1 tag + 32, occupied even when None) +
    // padding (2). Addresses begin at 56.
    let mut data = Vec::new();
    data.extend_from_slice(&1u32.to_le_bytes()); // ProgramState::LookupTable
    data.extend_from_slice(&u64::MAX.to_le_bytes()); // deactivation_slot = never
    data.extend_from_slice(&0u64.to_le_bytes()); // last_extended_slot
    // Addresses are only usable once current_slot > last_extended_slot;
    // before that only addresses[0..start_index] count as active. A frozen,
    // long-published table has every address predating its last extension, so
    // start_index is the full length — which is also what makes this usable at
    // litesvm's genesis slot.
    data.push(addresses.len() as u8); // last_extended_slot_start_index
    data.push(0); // authority = None — FROZEN, which is the whole point
    data.extend_from_slice(&[0u8; 32]); // the authority's slot, unused when None
    data.extend_from_slice(&[0u8; 2]); // padding
    assert_eq!(data.len(), 56, "lookup table meta must be exactly 56 bytes");
    for a in &addresses {
        data.extend_from_slice(a.as_ref());
    }
    w.svm.set_account(key, solana_sdk::account::Account {
        lamports: 10_000_000_000,
        data,
        owner: solana_sdk::address_lookup_table::program::ID,
        executable: false,
        rent_epoch: 0,
    }).unwrap();
    AddressLookupTableAccount { key, addresses }
}

/// The legacy form MUST be over budget. Pinned so nobody "simplifies" the
/// client back to a legacy transaction and discovers this in production.
#[test]
fn buyback_pnft_v2_legacy_tx_is_over_budget() {
    let mut w = setup(true);
    let v = setup_v2(&mut w);
    let p = mint_prize_to_vault(&mut w);
    let price = 10_000_000u64;

    let digest = cc_quote_digest(&w.vault, &p.mint, &v.destination, &v.usdc_mint, price, i64::MAX, 1, CC_BUYBACK_MEMO);
    let tx = Transaction::new_signed_with_payer(
        &[cc_ed25519_ix(&v.quote_signer, &digest),
          ComputeBudgetInstruction::set_compute_unit_limit(500_000),
          buyback_pnft_v2_ix(&w, &v, &p, price, 1, i64::MAX),
          memo_ix(CC_BUYBACK_MEMO)],
        Some(&w.hot.pubkey()), &[&w.hot], w.svm.latest_blockhash());

    let size = 1 + 64 + tx.message_data().len();
    println!("buyback_pnft_v2 as a LEGACY transaction: {size} bytes (cap {PACKET_DATA_SIZE})");
    assert!(size > PACKET_DATA_SIZE,
        "legacy now fits ({size} <= {PACKET_DATA_SIZE}) — the ALT may no longer be needed, revisit the client contract");
}

/// The v0 form with CC's frozen lookup table, and the full swap executing.
#[test]
fn buyback_pnft_v2_v0_atomic_swap_and_packet_budget() {
    let mut w = setup(true);
    let v = setup_v2(&mut w);
    let p = mint_prize_to_vault(&mut w);
    let price = 10_000_000u64;

    // Everything constant across every buyback goes in the table. The prize,
    // the vault and the destination stay static: a new prize wallet must never
    // require extending a frozen table.
    let cc_op = w.cc_operator.pubkey();
    let alt = plant_alt(&mut w, &v, vec![
        Pubkey::find_program_address(&[CONFIG_SEED], &PROGRAM_ID).0,
        cc_op,
        v.usdc_mint,
        v.cc_usdc,
        CC_RULE_SET,
        AUTH_RULES_PROGRAM_ID,
        mpl_token_metadata::ID,
        solana_sdk::sysvar::instructions::ID,
        ATA_PROGRAM_ID,
        CC_BUYBACK_ID,
        v.cc_policy,
        TOKEN_PROGRAM_ID,
        system_program::ID,
    ]);

    let digest = cc_quote_digest(&w.vault, &p.mint, &v.destination, &v.usdc_mint, price, i64::MAX, 1, CC_BUYBACK_MEMO);
    let ixs = vec![
        cc_ed25519_ix(&v.quote_signer, &digest),
        ComputeBudgetInstruction::set_compute_unit_limit(500_000),
        buyback_pnft_v2_ix(&w, &v, &p, price, 1, i64::MAX),
        memo_ix(CC_BUYBACK_MEMO),
    ];
    let msg = v0::Message::try_compile(&w.hot.pubkey(), &ixs, &[alt], w.svm.latest_blockhash())
        .expect("v0 compile against the lookup table");
    let vtx = VersionedTransaction::try_new(VersionedMessage::V0(msg), &[&w.hot]).unwrap();

    let size = bincode_len(&vtx);
    println!("buyback_pnft_v2 as a V0 transaction with the ALT: {size} bytes (cap {PACKET_DATA_SIZE})");
    assert!(size <= PACKET_DATA_SIZE, "v0 + ALT must fit one packet ({size} > {PACKET_DATA_SIZE})");

    let vault_usdc_before = spl_amount(&w, &v.vault_usdc);
    w.svm.send_transaction(vtx).expect("v0 atomic buyback should land");

    assert_eq!(spl_amount(&w, &v.vault_usdc), vault_usdc_before + price, "vault was not paid");
    assert_eq!(spl_amount(&w, &ata(&v.destination, &p.mint)), 1, "prize did not reach the destination");
    // Replay protection is cc_buyback's marker now, not a counter here.
    let digest = cc_quote_digest(&w.vault, &p.mint, &v.destination, &v.usdc_mint, price, i64::MAX, 1, CC_BUYBACK_MEMO);
    assert!(
        w.svm.get_account(&cc_quote_marker_pda(&digest)).is_some_and(|a| !a.data.is_empty()),
        "cc_buyback did not mark the quote spent",
    );
    assert_eq!(get_vault(&w).buyback_nonce, 0, "v2 must not touch buyback_nonce — it is layout padding now");
    assert!(w.svm.get_account(&p.token).map_or(true, |a| a.data.is_empty()),
        "prize ATA should have been closed");
}

/// CC fronted this prize's ATA rent at delivery, and cc_buyback will not pay
/// unless it comes back to the address CC's OWN policy names. Reading this
/// program's source is not the guarantee — it can be redeployed — so the same
/// swap is run with cc_buyback pointed at a different rent destination and must
/// be refused. Everything else is identical to the passing case, so the only
/// reachable failure is the rent check.
#[test]
fn buyback_pnft_v2_rejects_rent_sent_to_the_wrong_wallet() {
    let mut w = setup(true);
    let v = setup_v2(&mut w);
    let p = mint_prize_to_vault(&mut w);
    let price = 10_000_000u64;

    // Same destination, same lane, same signer — only rent_destination moves.
    let (cc_policy, cc_bump) = cc_policy_pda();
    plant_cc_policy(
        &mut w.svm,
        cc_policy,
        cc_bump,
        v.quote_signer.pubkey(),
        &[v.destination],
        &[CcLane { mint: v.usdc_mint, treasury: v.cc_usdc }],
        Pubkey::new_unique(),
    );

    let cc_op = w.cc_operator.pubkey();
    let alt = plant_alt(&mut w, &v, vec![
        Pubkey::find_program_address(&[CONFIG_SEED], &PROGRAM_ID).0,
        cc_op,
        v.usdc_mint,
        v.cc_usdc,
        CC_RULE_SET,
        AUTH_RULES_PROGRAM_ID,
        mpl_token_metadata::ID,
        solana_sdk::sysvar::instructions::ID,
        ATA_PROGRAM_ID,
        CC_BUYBACK_ID,
        v.cc_policy,
        TOKEN_PROGRAM_ID,
        system_program::ID,
    ]);

    let ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(500_000),
        cc_ed25519_ix(
            &v.quote_signer,
            &cc_quote_digest(&w.vault, &p.mint, &v.destination, &v.usdc_mint, price, i64::MAX, 1, CC_BUYBACK_MEMO),
        ),
        buyback_pnft_v2_ix(&w, &v, &p, price, 1, i64::MAX),
        memo_ix(CC_BUYBACK_MEMO),
    ];
    let msg = v0::Message::try_compile(&w.hot.pubkey(), &ixs, &[alt], w.svm.latest_blockhash())
        .expect("v0 compile against the lookup table");
    let vtx = VersionedTransaction::try_new(VersionedMessage::V0(msg), &[&w.hot]).unwrap();

    let err = w.svm.send_transaction(vtx).expect_err("rent went elsewhere; must not pay");
    let logs = err.meta.logs.join("\n");
    assert!(
        logs.contains("RentNotReturned"),
        "expected RentNotReturned, got:\n{logs}"
    );
    // And nothing moved.
    assert_eq!(spl_amount(&w, &v.vault_usdc), 0, "vault must not have been paid");
}

/// CC is not a signer, so the phone's is the only signature on the wire.
#[test]
fn buyback_pnft_v2_needs_only_the_phone_signature() {
    let mut w = setup(true);
    let v = setup_v2(&mut w);
    let p = mint_prize_to_vault(&mut w);
    let ix = buyback_pnft_v2_ix(&w, &v, &p, 10_000_000, 1, i64::MAX);
    let signers: Vec<Pubkey> = ix.accounts.iter().filter(|m| m.is_signer).map(|m| m.pubkey).collect();
    assert_eq!(signers, vec![w.hot.pubkey()], "someone other than the phone must sign");
}

/// A quote naming a destination CC has not allow-listed. In v1 this slot held
/// `rejects_a_stale_nonce`; the vault no longer keeps a counter, so the
/// equivalent guard is cc_buyback's own — covered by
/// `a_returned_asset_cannot_be_bought_twice_with_one_quote` in cc_buyback_v2.rs.
#[test]
fn buyback_pnft_v2_rejects_an_unlisted_destination() {
    let mut w = setup(true);
    let mut v = setup_v2(&mut w);
    let p = mint_prize_to_vault(&mut w);
    let price = 10_000_000u64;

    // Re-point the quote (and the instruction) at a destination the policy does
    // not list. The digest still verifies; cc_buyback refuses on the allow-list.
    v.destination = Pubkey::new_unique();
    let digest = cc_quote_digest(&w.vault, &p.mint, &v.destination, &v.usdc_mint, price, i64::MAX, 1, CC_BUYBACK_MEMO);
    let cc_op = w.cc_operator.pubkey();
    let alt = plant_alt(&mut w, &v, vec![
        Pubkey::find_program_address(&[CONFIG_SEED], &PROGRAM_ID).0, cc_op,
        v.usdc_mint, v.cc_usdc, CC_RULE_SET, AUTH_RULES_PROGRAM_ID, mpl_token_metadata::ID,
        solana_sdk::sysvar::instructions::ID, ATA_PROGRAM_ID, CC_BUYBACK_ID, v.cc_policy,
        TOKEN_PROGRAM_ID, system_program::ID,
    ]);
    let ixs = vec![
        cc_ed25519_ix(&v.quote_signer, &digest),
        ComputeBudgetInstruction::set_compute_unit_limit(500_000),
        buyback_pnft_v2_ix(&w, &v, &p, price, 1, i64::MAX),
    ];
    let msg = v0::Message::try_compile(&w.hot.pubkey(), &ixs, &[alt], w.svm.latest_blockhash()).unwrap();
    let vtx = VersionedTransaction::try_new(VersionedMessage::V0(msg), &[&w.hot]).unwrap();

    assert!(w.svm.send_transaction(vtx).is_err(), "an unlisted destination was accepted");
    assert_eq!(spl_amount(&w, &p.token), 1, "prize moved on a rejected buyback");
}

fn bincode_len(tx: &VersionedTransaction) -> usize {
    // Same accounting the legacy assertions use: shortvec(sig count) + 64 per
    // signature + the serialized message.
    let msg = tx.message.serialize();
    1 + tx.signatures.len() * 64 + msg.len()
}
