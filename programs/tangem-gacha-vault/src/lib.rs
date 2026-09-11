use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    instruction::{AccountMeta, Instruction},
    program::{invoke_signed},
    sysvar,
};
use anchor_spl::token_interface::{
    self, CloseAccount, Mint, TokenAccount, TokenInterface, TransferChecked,
};
use mpl_token_metadata::instructions::{
    DelegateTransferV1CpiBuilder, RevokeTransferV1CpiBuilder, TransferV1CpiBuilder,
};

// Synced with target/deploy/tangem_gacha_vault-keypair.json (anchor keys sync).
declare_id!("29agFEruMu2jedVwnDgKPuq7ejmTiB7sEqzdhufQQS9u");

pub const CONFIG_SEED: &[u8] = b"config";
pub const VAULT_SEED: &[u8] = b"vault";
pub const MAX_MEMO_LEN: usize = 256;
pub const SECONDS_PER_DAY: i64 = 86_400;
/// Hard ceiling on the Tangem fee so a compromised admin key cannot set a
/// confiscatory fee (10%).
pub const MAX_FEE_BPS: u16 = 1_000;
/// Metaplex Core — the asset standard most current CC prizes use (older
/// prizes are Token Metadata pNFTs).
pub const MPL_CORE_ID: Pubkey =
    anchor_lang::solana_program::pubkey!("CoREENxT6tW1HoK8ypY1SxRMZTcVPm7R94rH4PZNhX7d");
/// mpl-token-auth-rules — evaluates the pNFT rule set during Token Metadata
/// transfers/delegations. Pinned rather than left to the CPI to check, so the
/// program's account-pinning discipline has no exceptions.
pub const AUTH_RULES_ID: Pubkey =
    anchor_lang::solana_program::pubkey!("auth9SigNpDKz4sJJ1DfCTuZrZNSAgh9sFD3rboVmgg");

/// Collector Crypt's buyback authorization program. A COMPILE-TIME constant,
/// deliberately not a Config field: no admin key can repoint the thing that
/// decides whether a buyback was authorized.
pub const CC_BUYBACK_ID: Pubkey =
    anchor_lang::solana_program::pubkey!("CcBuyM7sDhedBGLZxivBvgZVdqzrQAG66KYHgnTTEpLF");
/// Seed of cc_buyback's singleton policy PDA.
pub const CC_POLICY_SEED: &[u8] = b"policy";
/// cc_buyback's per-quote replay marker, `[b"quote", digest]`. The digest is not
/// known here, so the address is passed in and cc_buyback checks it itself.
pub const CC_QUOTE_SEED: &[u8] = b"quote";
/// cc_buyback's rent vault, `[b"rent"]` — it funds the marker, so the phone does
/// not have to.
pub const CC_RENT_VAULT_SEED: &[u8] = b"rent";
/// sha256("global:authorize_and_pay")[..8]. Hand-encoded rather than taken as a
/// Cargo dependency, matching how the mpl-core CPIs are built here; the
/// integration tests pin the byte sequence so a rename on CC's side fails loudly.
pub const CC_AUTHORIZE_AND_PAY_IX: [u8; 8] = [196, 1, 233, 204, 98, 232, 22, 54];

/// Per-user delegated-custody vault for the Collector Crypt Gacha integration.
///
/// Integration facts this design is built on:
/// - CC's gacha has NO on-chain program. `generatePack` returns a backend-built
///   tx (the gacha wallet pre-signs as FEE PAYER;
///   the memo instruction itself carries no accounts): top-level memo
///   `<slug>-<uuid>:open` + top-level USDC transfer from the player's ATA to
///   their treasury. The API's `memo` field is the bare `<slug>-<uuid>` join
///   key; the `:open` suffix is appended when the tx is built. We DISCARD this
///   tx and pay from the vault instead (see the spin path below).
/// - Prizes are MOSTLY Metaplex Core assets now (AssetV1, collection
///   CCryptUfeFSZ3Fgc9FLeKrhLVAP67FSqi1GuVoj9CRac — see withdraw_core); older
///   prizes are Token Metadata pNFTs (TokenStandard=4, collection
///   CCryptWBYktukHDQ2vHGtVcmtjXxYzvw8XNVY64YN2Yf). pNFT token accounts are
///   permanently frozen: plain spl-token transfer/approve are impossible; all
///   moves go through Token Metadata `TransferV1`, delegation through
///   `DelegateV1` (TokenDelegateRole::Transfer, stored in the TokenRecord and
///   cleared automatically on transfer).
/// - A PDA CAN receive prize NFTs (recipient never signs the `:send` leg,
///   `altPlayerAddress` is an explicit API param), and buyback eligibility is
///   any current holder within ~3 days of the prize leaving the gacha.
///   `/api/buyback` takes an optional `transferAuthority` and builds for
///   off-curve owners, and CC scopes a no-closeAccount template to our API
///   key — so the legacy path (`approve_buyback` + CC's template) works today.
///   It exists ONLY until CC's backend switches to `buyback_pnft` /
///   `buyback_core` below; once it does, `approve_buyback` becomes removable
///   and `allow_buyback_delegation` should be set false permanently, which
///   retires the standing-delegate drain risk entirely.
///
/// The spin path: `open_pack` pays the CC treasury from the vault via an
/// inner CPI transfer; the client adds a top-level `<memo>:open` memo
/// instruction, and CC's webhook credits the spin by matching memo + treasury
/// transfer (inner instructions included; no gacha-wallet co-signature, no
/// payer registration needed).
///
/// Trust model (honest worst case):
/// - `cold_owner` (Tangem card key) is the root authority: rotate the hot
///   delegate, change caps, withdraw any asset anywhere, close the vault.
/// - `hot_delegate` (phone key) can:
///   (a) pay for spins within the per-spin/daily caps, and ONLY to the
///       whitelisted treasury accounts fixed in the config. Note what the caps
///       do NOT bound: the prize RECIPIENT is `altPlayerAddress`, chosen
///       off-chain when the memo is minted, and CC credits whichever memo
///       accompanies a treasury transfer regardless of who paid. A compromised
///       hot key can therefore spend the vault's USDC on spins whose prizes are
///       delivered to ITS OWN wallet, realizing the caps as value, not as
///       waste. Size `daily_cap` as a theft budget. The only binding is
///       operational: the Tangem backend must mint memos exclusively with
///       `altPlayerAddress` = the vault PDA, index every `PackOpened.memo`, and
///       alarm on a memo it did not issue or a prize not delivered to the PDA.
///   (b) take a transfer delegate over vault-held prize pNFTs one at a time.
///       A transfer delegate can move that pNFT ANYWHERE (the CC ruleset blocks
///       nothing), so a compromised hot key can drain vault pNFTs one-by-one by
///       cycling approve → transfer → clear. The one-live-delegate slot and the
///       `allow_buyback_delegation` kill switch bound the rate of THIS path
///       only; `revoke_buyback` and `update_vault` (rotate key) are the
///       reactive controls.
///   (c) co-sign `buyback_pnft` / `buyback_core` (see below).
/// - Tangem admin key: config only. `config.rent_destination` (formerly
///   `gacha_wallet`) is now just that — where CC's fronted prize-ATA rent goes
///   back. It is NOT an asset-custody authority: buybacks are authorized by a
///   CC-signed quote verified against the cc_buyback policy, so no key held by
///   Tangem, and no key held by CC either, can move a prize on its own
///   signature. `price` is bound to the quote, `destination_owner` must be on
///   CC's on-chain allow-list, and each quote is spendable once against
///   cc_buyback's own `[b"quote", digest]` marker. Hold the admin key in a
///   multisig (Squads) — still a prerequisite — and alarm on `ConfigUpdated`,
///   treating a change to `gacha_usdc_account` or `fee_usdc_account` as
///   break-glass. Incident levers, in order of bluntness: CC's own `set_paused`
///   on the buyback policy (stops buybacks globally while spins keep working);
///   `paused` here (stops spins and buybacks for everyone); per vault, a
///   cold-key `update_vault` hot rotation. `allow_buyback_delegation` gates
///   `approve_buyback` ONLY and is not a brake on the atomic buybacks.
///   The admin key still reaches value on a SECOND, independent path: the same
///   one-signature `update_config` rewrites `gacha_usdc_account` and
///   `fee_usdc_account`, and those two pins are the only destination checks
///   `open_pack` performs. One admin write therefore diverts 100% of every
///   FUTURE spin payment — principal and fee, every vault, Config being a
///   singleton — to an attacker-owned USDC account, with no hot key involved:
///   users keep spinning, CC never credits the spins, the money lands
///   elsewhere. `MAX_FEE_BPS` bounds the fee rate, not this.
/// - Collector Crypt: for Core prizes CC holds collection-level
///   PermanentTransferDelegate/PermanentBurnDelegate, so it can seize or burn
///   them regardless of custody. The vault protects Core prizes against key
///   theft, not against CC.
#[program]
pub mod tangem_gacha_vault {
    use super::*;

    /// One-time global setup. Only the program's upgrade authority can call
    /// this (prevents config-admin front-running between deploy and init).
    pub fn initialize_config(
        ctx: Context<InitializeConfig>,
        rent_destination: Pubkey,
        gacha_usdc_account: Pubkey,
        fee_usdc_account: Pubkey,
        fee_bps: u16,
        allow_buyback_delegation: bool,
    ) -> Result<()> {
        require!(fee_bps <= MAX_FEE_BPS, VaultError::InvalidFeeBps);
        // `usdc_mint` is written here and NOWHERE else — `update_config` has no
        // parameter for it and the config PDA can neither be closed nor
        // re-initialized, so this choice is permanent for the life of the
        // program id. Narrow it to legacy SPL Token, the only standard the
        // suite exercises: a Token-2022 mint with a transfer-fee or
        // transfer-hook extension would silently short every `transfer_checked`
        // in this program (a spin that under-delivers to the treasury is a paid
        // spin CC never credits), and there would be no way to undo it.
        require_keys_eq!(
            *ctx.accounts.usdc_mint.to_account_info().owner,
            anchor_spl::token::ID,
            VaultError::UnsupportedMint
        );
        let config = &mut ctx.accounts.config;
        config.admin = ctx.accounts.admin.key();
        config.pending_admin = Pubkey::default();
        config.usdc_mint = ctx.accounts.usdc_mint.key();
        config.rent_destination = rent_destination;
        config.gacha_usdc_account = gacha_usdc_account;
        config.fee_usdc_account = fee_usdc_account;
        config.fee_bps = fee_bps;
        config.paused = false;
        config._reserved = false;
        config.allow_buyback_delegation = allow_buyback_delegation;
        config.bump = ctx.bumps.config;
        config._padding = [0u8; 128];
        Ok(())
    }

    /// Admin-only partial updates of the global config.
    ///
    /// `pending_admin` NOMINATES a successor; it does not hand over. The
    /// nominee must call `accept_admin`. Passing the default pubkey cancels a
    /// pending nomination.
    #[allow(clippy::too_many_arguments)]
    pub fn update_config(
        ctx: Context<UpdateConfig>,
        pending_admin: Option<Pubkey>,
        rent_destination: Option<Pubkey>,
        gacha_usdc_account: Option<Pubkey>,
        fee_usdc_account: Option<Pubkey>,
        fee_bps: Option<u16>,
        paused: Option<bool>,
        allow_buyback_delegation: Option<bool>,
    ) -> Result<()> {
        let config = &mut ctx.accounts.config;
        if let Some(v) = pending_admin {
            config.pending_admin = v;
        }
        if let Some(v) = rent_destination {
            config.rent_destination = v;
        }
        if let Some(v) = gacha_usdc_account {
            config.gacha_usdc_account = v;
        }
        if let Some(v) = fee_usdc_account {
            config.fee_usdc_account = v;
        }
        if let Some(v) = fee_bps {
            require!(v <= MAX_FEE_BPS, VaultError::InvalidFeeBps);
            config.fee_bps = v;
        }
        if let Some(v) = paused {
            config.paused = v;
        }
        if let Some(v) = allow_buyback_delegation {
            config.allow_buyback_delegation = v;
        }
        emit!(ConfigUpdated {
            admin: config.admin,
            pending_admin: config.pending_admin,
            rent_destination: config.rent_destination,
            gacha_usdc_account: config.gacha_usdc_account,
            fee_usdc_account: config.fee_usdc_account,
            fee_bps: config.fee_bps,
            paused: config.paused,
            allow_buyback_delegation: config.allow_buyback_delegation,
        });
        Ok(())
    }

    /// Second half of the admin handover: the nominee claims the role. Until
    /// this lands the sitting admin is unchanged, so a nomination sent to a
    /// mistyped or unusable address costs nothing and is cancelled by
    /// nominating the default pubkey.
    pub fn accept_admin(ctx: Context<AcceptAdmin>) -> Result<()> {
        let config = &mut ctx.accounts.config;
        let previous = config.admin;
        config.admin = ctx.accounts.pending_admin.key();
        config.pending_admin = Pubkey::default();
        emit!(AdminTransferred {
            previous,
            current: config.admin,
        });
        Ok(())
    }

    /// Creates the per-user vault. The Tangem card taps ONCE here (cold_owner
    /// signature); the phone wallet pays rent so the cold-signed message stays
    /// minimal (Tangem cards have a stricter transaction-size budget than
    /// Solana itself).
    pub fn init_vault(
        ctx: Context<InitVault>,
        hot_delegate: Pubkey,
        per_spin_cap: u64,
        daily_cap: u64,
    ) -> Result<()> {
        require!(
            per_spin_cap > 0 && daily_cap >= per_spin_cap,
            VaultError::InvalidCaps
        );
        let vault = &mut ctx.accounts.vault;
        vault.cold_owner = ctx.accounts.cold_owner.key();
        vault.hot_delegate = hot_delegate;
        vault.per_spin_cap = per_spin_cap;
        vault.daily_cap = daily_cap;
        vault.spent_today = 0;
        vault.day_index = Clock::get()?.unix_timestamp.div_euclid(SECONDS_PER_DAY);
        vault.buyback_nonce = 0;
        vault.live_buyback_mint = None;
        vault.live_buyback_token = None;
        vault._padding = [0u8; 128];
        vault.bump = ctx.bumps.vault;
        emit!(VaultInitialized {
            vault: vault.key(),
            cold_owner: vault.cold_owner,
            hot_delegate,
        });
        Ok(())
    }

    /// Cold-key-only: rotate the hot delegate and/or adjust caps.
    pub fn update_vault(
        ctx: Context<ColdAuthority>,
        hot_delegate: Option<Pubkey>,
        per_spin_cap: Option<u64>,
        daily_cap: Option<u64>,
    ) -> Result<()> {
        let vault = &mut ctx.accounts.vault;
        if let Some(v) = hot_delegate {
            vault.hot_delegate = v;
            emit!(HotDelegateRotated {
                vault: vault.key(),
                hot_delegate: v
            });
        }
        if let Some(v) = per_spin_cap {
            vault.per_spin_cap = v;
        }
        if let Some(v) = daily_cap {
            vault.daily_cap = v;
        }
        require!(
            vault.per_spin_cap > 0 && vault.daily_cap >= vault.per_spin_cap,
            VaultError::InvalidCaps
        );
        Ok(())
    }

    /// Pay for one gacha spin straight from the vault, signed only by the
    /// hot key.
    ///
    /// Sends `amount` USDC from the vault to the whitelisted Collector Crypt
    /// treasury token account. The `memo` argument must be
    /// `<memo returned by /api/generatePack>` + `":open"` — the API field is
    /// the bare `<slug>-<uuid>` join key; the BACKEND appends the suffix. A
    /// Tangem fee of `fee_bps` is charged ON TOP of `amount` into the fee
    /// treasury, atomically.
    ///
    /// CLIENT CONTRACT: the transaction MUST carry a TOP-LEVEL spl-memo
    /// instruction with the same string — that memo plus the treasury
    /// transfer (inner CPI transfers are parsed) is what credits the spin.
    /// No gacha-wallet co-signature and no payer registration are needed;
    /// submit via any RPC (CC's /api/submitTransaction rejects foreign
    /// signatures). The
    /// `memo` arg here is validated and recorded in the PackOpened event for
    /// reconciliation; the program does NOT emit its own memo instruction —
    /// CC's indexer only reads the top-level one.
    pub fn open_pack(ctx: Context<OpenPack>, amount: u64, memo: String) -> Result<()> {
        let config = &ctx.accounts.config;
        require!(!config.paused, VaultError::Paused);
        require!(amount > 0, VaultError::ZeroAmount);
        require!(
            !memo.is_empty() && memo.len() <= MAX_MEMO_LEN,
            VaultError::InvalidMemo
        );

        let fee = compute_fee(amount, config.fee_bps)?;
        let total = amount.checked_add(fee).ok_or(VaultError::MathOverflow)?;
        charge_spending(&mut ctx.accounts.vault, total)?;

        let vault_key = ctx.accounts.vault.key();
        let seeds: &[&[u8]] = &[
            VAULT_SEED,
            ctx.accounts.vault.cold_owner.as_ref(),
            &[ctx.accounts.vault.bump],
        ];
        let signer_seeds: &[&[&[u8]]] = &[seeds];

        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.vault_usdc.to_account_info(),
                    mint: ctx.accounts.usdc_mint.to_account_info(),
                    to: ctx.accounts.gacha_usdc.to_account_info(),
                    authority: ctx.accounts.vault.to_account_info(),
                },
                signer_seeds,
            ),
            amount,
            ctx.accounts.usdc_mint.decimals,
        )?;

        if fee > 0 {
            token_interface::transfer_checked(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    TransferChecked {
                        from: ctx.accounts.vault_usdc.to_account_info(),
                        mint: ctx.accounts.usdc_mint.to_account_info(),
                        to: ctx.accounts.fee_usdc.to_account_info(),
                        authority: ctx.accounts.vault.to_account_info(),
                    },
                    signer_seeds,
                ),
                fee,
                ctx.accounts.usdc_mint.decimals,
            )?;
        }

        emit!(PackOpened {
            vault: vault_key,
            cold_owner: ctx.accounts.vault.cold_owner,
            amount,
            fee,
            memo,
        });
        Ok(())
    }

    /// Hot-delegate action: take a Metaplex Token Metadata TRANSFER delegate
    /// (TokenDelegateRole::Transfer, amount = 1) over ONE vault-held prize
    /// pNFT, so the hot key can countersign Collector Crypt's buyback
    /// transaction as the transfer authority (requires CC to build the buyback
    /// tx with authority = delegate, fee payer = the hot key, and without the
    /// closeAccount instruction; `altRecipient` points the USDC refund at the
    /// vault).
    ///
    /// HONEST TRADE-OFF: a transfer delegate can move the pNFT to ANY address
    /// (the CC ruleset blocks nothing), entirely outside this program. Only
    /// one delegate may be live per vault at a time (`live_buyback_mint`), so
    /// a compromised hot key drains prizes serially, not in one shot — the
    /// slot frees only when the previous NFT provably left or was revoked
    /// (`clear_buyback_slot` / `revoke_buyback`). The admin kill switch is
    /// `allow_buyback_delegation`; the cold-side controls are `revoke_buyback`
    /// and rotating the hot key.
    pub fn approve_buyback(ctx: Context<ApproveBuyback>) -> Result<()> {
        require_auth_rules_program(&ctx.accounts.authorization_rules_program)?;
        let config = &ctx.accounts.config;
        require!(!config.paused, VaultError::Paused);
        require!(
            config.allow_buyback_delegation,
            VaultError::BuybackDelegationDisabled
        );
        require!(ctx.accounts.nft_token.amount == 1, VaultError::NotAnNft);
        require!(
            ctx.accounts.nft_mint.key() != config.usdc_mint,
            VaultError::NotAnNft
        );
        require!(
            ctx.accounts.vault.live_buyback_mint.is_none(),
            VaultError::BuybackSlotOccupied
        );

        let seeds: &[&[u8]] = &[
            VAULT_SEED,
            ctx.accounts.vault.cold_owner.as_ref(),
            &[ctx.accounts.vault.bump],
        ];

        // Bind every account as an owned AccountInfo local so the CPI-builder
        // Option args receive `Option<&AccountInfo>` (deref coercion does not
        // reach through `Some(..)`).
        let tmp = ctx.accounts.token_metadata_program.to_account_info();
        let delegate_info = ctx.accounts.hot_delegate.to_account_info();
        let metadata_info = ctx.accounts.metadata.to_account_info();
        let edition_info = ctx.accounts.edition.to_account_info();
        let token_record_info = ctx.accounts.token_record.to_account_info();
        let vault_info = ctx.accounts.vault.to_account_info();
        let mint_info = ctx.accounts.nft_mint.to_account_info();
        let token_info = ctx.accounts.nft_token.to_account_info();
        let payer_info = ctx.accounts.hot_delegate.to_account_info();
        let system_info = ctx.accounts.system_program.to_account_info();
        let sysvar_info = ctx.accounts.sysvar_instructions.to_account_info();
        let token_program_info = ctx.accounts.token_program.to_account_info();
        let auth_rules_info = ctx
            .accounts
            .authorization_rules
            .as_ref()
            .map(|a| a.to_account_info());
        let auth_rules_program_info = ctx
            .accounts
            .authorization_rules_program
            .as_ref()
            .map(|a| a.to_account_info());

        DelegateTransferV1CpiBuilder::new(&tmp)
            .delegate(&delegate_info)
            .metadata(&metadata_info)
            .master_edition(Some(&edition_info))
            .token_record(Some(&token_record_info))
            .mint(&mint_info)
            .token(&token_info)
            .authority(&vault_info)
            .payer(&payer_info)
            .system_program(&system_info)
            .sysvar_instructions(&sysvar_info)
            .spl_token_program(Some(&token_program_info))
            .authorization_rules_program(auth_rules_program_info.as_ref())
            .authorization_rules(auth_rules_info.as_ref())
            .amount(1)
            .invoke_signed(&[seeds])?;

        ctx.accounts.vault.live_buyback_mint = Some(ctx.accounts.nft_mint.key());
        ctx.accounts.vault.live_buyback_token = Some(ctx.accounts.nft_token.key());

        emit!(BuybackApproved {
            vault: ctx.accounts.vault.key(),
            nft_mint: ctx.accounts.nft_mint.key(),
        });
        Ok(())
    }

    /// Revoke the pNFT transfer delegate and free the buyback slot. Callable
    /// by cold owner or hot delegate.
    pub fn revoke_buyback(ctx: Context<RevokeBuyback>) -> Result<()> {
        require_auth_rules_program(&ctx.accounts.authorization_rules_program)?;
        let authority = ctx.accounts.authority.key();
        require!(
            authority == ctx.accounts.vault.cold_owner
                || authority == ctx.accounts.vault.hot_delegate,
            VaultError::Unauthorized
        );

        let seeds: &[&[u8]] = &[
            VAULT_SEED,
            ctx.accounts.vault.cold_owner.as_ref(),
            &[ctx.accounts.vault.bump],
        ];

        let tmp = ctx.accounts.token_metadata_program.to_account_info();
        let delegate_info = ctx.accounts.delegate.to_account_info();
        let metadata_info = ctx.accounts.metadata.to_account_info();
        let edition_info = ctx.accounts.edition.to_account_info();
        let token_record_info = ctx.accounts.token_record.to_account_info();
        let vault_info = ctx.accounts.vault.to_account_info();
        let mint_info = ctx.accounts.nft_mint.to_account_info();
        let token_info = ctx.accounts.nft_token.to_account_info();
        let payer_info = ctx.accounts.authority.to_account_info();
        let system_info = ctx.accounts.system_program.to_account_info();
        let sysvar_info = ctx.accounts.sysvar_instructions.to_account_info();
        let token_program_info = ctx.accounts.token_program.to_account_info();
        let auth_rules_info = ctx
            .accounts
            .authorization_rules
            .as_ref()
            .map(|a| a.to_account_info());
        let auth_rules_program_info = ctx
            .accounts
            .authorization_rules_program
            .as_ref()
            .map(|a| a.to_account_info());

        RevokeTransferV1CpiBuilder::new(&tmp)
            .delegate(&delegate_info)
            .metadata(&metadata_info)
            .master_edition(Some(&edition_info))
            .token_record(Some(&token_record_info))
            .mint(&mint_info)
            .token(&token_info)
            .authority(&vault_info)
            .payer(&payer_info)
            .system_program(&system_info)
            .sysvar_instructions(&sysvar_info)
            .spl_token_program(Some(&token_program_info))
            .authorization_rules_program(auth_rules_program_info.as_ref())
            .authorization_rules(auth_rules_info.as_ref())
            .invoke_signed(&[seeds])?;

        if ctx.accounts.vault.live_buyback_token == Some(ctx.accounts.nft_token.key()) {
            ctx.accounts.vault.live_buyback_mint = None;
            ctx.accounts.vault.live_buyback_token = None;
        }

        emit!(DelegateRevoked {
            vault: ctx.accounts.vault.key(),
            token_account: ctx.accounts.nft_token.key(),
        });
        Ok(())
    }

    /// Free the buyback slot after a COMPLETED buyback (the pNFT transfer
    /// consumed the delegate, so there is nothing to revoke). Requires
    /// on-chain proof against the EXACT account that was delegated
    /// (`live_buyback_token`): it must be empty or delegate-free. Presenting a
    /// different vault-owned account of the same mint cannot forge completion.
    /// Callable by cold owner or hot delegate.
    ///
    /// NOT needed after `buyback_pnft`, which frees the slot itself. Calling it
    /// then fails with Anchor `AccountNotInitialized` (3012) on `nft_token` —
    /// that ATA was closed in the same instruction, and the typed account is
    /// resolved before this handler runs, so `NoLiveBuyback` is never reached.
    pub fn clear_buyback_slot(ctx: Context<ClearBuybackSlot>) -> Result<()> {
        let authority = ctx.accounts.authority.key();
        let vault = &ctx.accounts.vault;
        require!(
            authority == vault.cold_owner || authority == vault.hot_delegate,
            VaultError::Unauthorized
        );
        let live_token = vault.live_buyback_token.ok_or(VaultError::NoLiveBuyback)?;
        require!(
            ctx.accounts.nft_token.key() == live_token,
            VaultError::WrongBuybackToken
        );
        require!(
            ctx.accounts.nft_token.amount == 0 || ctx.accounts.nft_token.delegate.is_none(),
            VaultError::BuybackStillActive
        );
        ctx.accounts.vault.live_buyback_mint = None;
        ctx.accounts.vault.live_buyback_token = None;
        emit!(BuybackSlotCleared {
            vault: ctx.accounts.vault.key(),
            token_account: ctx.accounts.nft_token.key(),
        });
        Ok(())
    }

    /// Atomic buyback of a prize pNFT, executed by Collector Crypt against
    /// the vault: CC pays `price` USDC into the vault's canonical USDC ATA
    /// and receives the NFT in the same instruction — the vault PDA signs the
    /// Token Metadata TransferV1 as the OWNER, so no standing delegate is
    /// involved. Two signatures: the CC operator wallet fixed in the config
    /// (pays the refund, the fees and the rent) and the hot key (user consent
    /// to the offered price). The NFT lands at `destination_owner` — CC's free
    /// per-transaction choice, since their prizes live across rotating prize
    /// wallets rather than on the operator wallet. The emptied prize ATA is
    /// then closed with its lamports returned to CC, who fronted that rent at
    /// delivery (the whole balance, not just the rent — see the close site), and a
    /// stale `approve_buyback` slot pointing at this token is freed.
    ///
    /// `memo` is CC's reconciliation key, recorded in the BuybackExecuted
    /// event (CC's webhook additionally reads its own top-level memo
    /// instruction, which their backend adds when building the transaction).
    ///
    /// CLIENT CONTRACT — the transaction is built by CC, so the phone's single
    /// signature is the only thing standing behind every instruction in it:
    /// - Prepend `AssociatedTokenAccount::CreateIdempotent(vault_usdc)` (payer
    ///   = `cc_authority`). `buyback_pnft` PINS the vault's canonical USDC ATA
    ///   but does NOT create it, so against a vault that never held USDC — or
    ///   whose ATA the cold owner closed at offboarding — `buyback_pnft` itself
    ///   aborts with `AccountNotInitialized` (3012).
    /// - Set a compute-unit limit of ~400k: the TransferV1 creates the
    ///   destination ATA + TokenRecord and evaluates the rule set, which does
    ///   not fit the 200k default.
    /// - The phone MUST decode the transaction before signing: display the
    ///   `price` ARGUMENT (not the quote the API rendered — nothing on-chain
    ///   bounds `price` beyond `> 0`), and refuse any transaction carrying an
    ///   instruction of this program other than the single buyback it is
    ///   showing. Consent is instruction-scoped, not transaction-scoped, and a
    ///   bundled `open_pack` would be signed by the very same hot key.
    ///
    /// SCOPE, HONESTLY: `nft_mint` carries no collection or creator pin — this
    /// moves ANY vault-held NFT, not only CC-issued prizes. The client is what
    /// keeps a non-CC asset the user parked in the vault out of scope.
    pub fn buyback_pnft(ctx: Context<BuybackPnft>, price: u64, memo: String) -> Result<()> {
        require_auth_rules_program(&ctx.accounts.authorization_rules_program)?;
        require!(!ctx.accounts.config.paused, VaultError::Paused);
        require!(price > 0, VaultError::ZeroAmount);
        require!(
            !memo.is_empty() && memo.len() <= MAX_MEMO_LEN,
            VaultError::InvalidMemo
        );
        require!(ctx.accounts.nft_token.amount == 1, VaultError::NotAnNft);
        require!(
            ctx.accounts.nft_mint.key() != ctx.accounts.config.usdc_mint,
            VaultError::NotAnNft
        );

        // The refund lands in the vault BEFORE the prize leaves it; a failed
        // payment aborts the whole instruction.
        token_interface::transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.cc_usdc.to_account_info(),
                    mint: ctx.accounts.usdc_mint.to_account_info(),
                    to: ctx.accounts.vault_usdc.to_account_info(),
                    authority: ctx.accounts.cc_authority.to_account_info(),
                },
            ),
            price,
            ctx.accounts.usdc_mint.decimals,
        )?;

        let seeds: &[&[u8]] = &[
            VAULT_SEED,
            ctx.accounts.vault.cold_owner.as_ref(),
            &[ctx.accounts.vault.bump],
        ];

        let tmp = ctx.accounts.token_metadata_program.to_account_info();
        let vault_info = ctx.accounts.vault.to_account_info();
        let mint_info = ctx.accounts.nft_mint.to_account_info();
        let token_info = ctx.accounts.nft_token.to_account_info();
        let dest_token_info = ctx.accounts.destination_token.to_account_info();
        let dest_owner_info = ctx.accounts.destination_owner.to_account_info();
        let metadata_info = ctx.accounts.metadata.to_account_info();
        let edition_info = ctx.accounts.edition.to_account_info();
        let token_record_info = ctx.accounts.token_record.to_account_info();
        let dest_token_record_info = ctx.accounts.destination_token_record.to_account_info();
        let payer_info = ctx.accounts.cc_authority.to_account_info();
        let system_info = ctx.accounts.system_program.to_account_info();
        let sysvar_info = ctx.accounts.sysvar_instructions.to_account_info();
        let token_program_info = ctx.accounts.token_program.to_account_info();
        let ata_program_info = ctx.accounts.ata_program.to_account_info();
        let auth_rules_info = ctx
            .accounts
            .authorization_rules
            .as_ref()
            .map(|a| a.to_account_info());
        let auth_rules_program_info = ctx
            .accounts
            .authorization_rules_program
            .as_ref()
            .map(|a| a.to_account_info());

        TransferV1CpiBuilder::new(&tmp)
            .token(&token_info)
            .token_owner(&vault_info)
            .destination_token(&dest_token_info)
            .destination_owner(&dest_owner_info)
            .mint(&mint_info)
            .metadata(&metadata_info)
            .edition(Some(&edition_info))
            .token_record(Some(&token_record_info))
            .destination_token_record(Some(&dest_token_record_info))
            .authority(&vault_info)
            .payer(&payer_info)
            .system_program(&system_info)
            .sysvar_instructions(&sysvar_info)
            .spl_token_program(&token_program_info)
            .spl_ata_program(&ata_program_info)
            .authorization_rules_program(auth_rules_program_info.as_ref())
            .authorization_rules(auth_rules_info.as_ref())
            .amount(1)
            .invoke_signed(&[seeds])?;

        // Close the emptied prize ATA; CC fronted its rent at delivery. This
        // forwards the account's WHOLE lamport balance, not just the rent —
        // deliberately unlike `sweep_prize_ata`, which closes into the vault and
        // pays out exactly the rent. The split is unnecessary here: that one is
        // permissionless, so a dust donation would be a free DoS on CC's cron,
        // whereas this path needs cc_authority + hot, a pair already documented
        // as able to take the prize itself for one micro-USDC. Any excess on a
        // prize ATA is a mis-send, and it rides out to cc_authority here.
        // It cannot be rescued "before the buyback": close_token_account
        // requires amount == 0 and the prize is still in the account. The
        // working order is withdraw_pnft (empties it) -> sweep_prize_ata
        // (returns exactly the rent to CC, leaves the excess in the vault)
        // -> withdraw_sol.
        token_interface::close_account(CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            CloseAccount {
                account: ctx.accounts.nft_token.to_account_info(),
                destination: ctx.accounts.cc_authority.to_account_info(),
                authority: ctx.accounts.vault.to_account_info(),
            },
            &[seeds],
        ))?;

        // A pending approve_buyback for this same NFT is consumed by the
        // owner-path transfer above — free the slot so it cannot wedge.
        if ctx.accounts.vault.live_buyback_token == Some(ctx.accounts.nft_token.key()) {
            ctx.accounts.vault.live_buyback_mint = None;
            ctx.accounts.vault.live_buyback_token = None;
        }

        emit!(BuybackExecuted {
            vault: ctx.accounts.vault.key(),
            mint: ctx.accounts.nft_mint.key(),
            price,
            memo,
        });
        Ok(())
    }

    /// Atomic buyback of a Metaplex CORE prize — same contract as
    /// `buyback_pnft` (CC operator wallet + hot key sign; USDC refund into
    /// the vault, then the asset moves), but a single light mpl-core transfer:
    /// the vault PDA signs as the asset owner, Core assets ship unfrozen, and
    /// there are no token accounts to garbage-collect. The asset lands at
    /// `destination_owner` — CC's free per-transaction choice (their prizes
    /// live across many rotating prize wallets, not on the operator wallet).
    ///
    /// The same CLIENT CONTRACT as `buyback_pnft` applies — create the vault's
    /// USDC ATA idempotently first, decode and display the `price` argument,
    /// and refuse a transaction carrying any other instruction of this program.
    /// The compute cost is a fraction of the pNFT path (one CPI, no ATA or
    /// TokenRecord creation), so the default budget suffices.
    ///
    /// SCOPE, HONESTLY: `asset` is pinned to be an mpl-core account and nothing
    /// more — mpl-core binds the collection to the asset, but neither is pinned
    /// to CC's collection, so this moves ANY Core asset the vault holds.
    pub fn buyback_core(ctx: Context<BuybackCore>, price: u64, memo: String) -> Result<()> {
        require!(!ctx.accounts.config.paused, VaultError::Paused);
        require!(price > 0, VaultError::ZeroAmount);
        require!(
            !memo.is_empty() && memo.len() <= MAX_MEMO_LEN,
            VaultError::InvalidMemo
        );

        token_interface::transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.cc_usdc.to_account_info(),
                    mint: ctx.accounts.usdc_mint.to_account_info(),
                    to: ctx.accounts.vault_usdc.to_account_info(),
                    authority: ctx.accounts.cc_authority.to_account_info(),
                },
            ),
            price,
            ctx.accounts.usdc_mint.decimals,
        )?;

        let seeds: &[&[u8]] = &[
            VAULT_SEED,
            ctx.accounts.vault.cold_owner.as_ref(),
            &[ctx.accounts.vault.bump],
        ];
        // mpl-core TransferV1, same wire format as withdraw_core. The new
        // owner is `destination_owner`, NOT the signing operator wallet — CC
        // returns bought-back prizes to rotating prize wallets.
        let metas = vec![
            AccountMeta::new(ctx.accounts.asset.key(), false),
            AccountMeta::new_readonly(ctx.accounts.collection.key(), false),
            AccountMeta::new(ctx.accounts.cc_authority.key(), true),
            AccountMeta::new_readonly(ctx.accounts.vault.key(), true),
            AccountMeta::new_readonly(ctx.accounts.destination_owner.key(), false),
            AccountMeta::new_readonly(MPL_CORE_ID, false),
            AccountMeta::new_readonly(MPL_CORE_ID, false),
        ];
        let ix = Instruction {
            program_id: MPL_CORE_ID,
            accounts: metas,
            data: vec![14, 0],
        };
        invoke_signed(
            &ix,
            &[
                ctx.accounts.asset.to_account_info(),
                ctx.accounts.collection.to_account_info(),
                ctx.accounts.cc_authority.to_account_info(),
                ctx.accounts.vault.to_account_info(),
                ctx.accounts.destination_owner.to_account_info(),
                ctx.accounts.mpl_core_program.to_account_info(),
            ],
            &[seeds],
        )?;

        emit!(BuybackExecuted {
            vault: ctx.accounts.vault.key(),
            mint: ctx.accounts.asset.key(),
            price,
            memo,
        });
        Ok(())
    }

    /// Buyback of a Metaplex Core prize, authorized by a CC-signed quote
    /// instead of a CC signature on the transaction.
    ///
    /// What changes versus `buyback_core`: Collector Crypt no longer signs
    /// anything here. The transaction carries a top-level ed25519 instruction
    /// holding CC's detached quote; this program CPIs into `cc_buyback`, which
    /// verifies that quote against its own policy and moves the payment. So
    /// `config.rent_destination` stops being an asset-custody authority, `price`
    /// is bound to the quote rather than merely `> 0`, `destination_owner` must
    /// be on CC's destination allow-list, and cc_buyback's own quote marker
    /// makes the quote
    /// spendable exactly once.
    ///
    /// The phone still co-signs, so the user still approves the specific price,
    /// and it is now also the fee payer and the mpl-core rent payer.
    ///
    /// CLIENT CONTRACT: prepend the ed25519 quote instruction CC returned, and
    /// display the `price` ARGUMENT — not the quote the API rendered.
    pub fn buyback_core_v2(
        ctx: Context<BuybackCoreV2>,
        price: u64,
        quote_id: u64,
        expires_at: i64,
        memo: String,
    ) -> Result<()> {
        require!(!ctx.accounts.config.paused, VaultError::Paused);
        require!(price > 0, VaultError::ZeroAmount);
        require!(
            !memo.is_empty() && memo.len() <= MAX_MEMO_LEN,
            VaultError::InvalidMemo
        );
        require_keys_eq!(
            ctx.accounts.cc_program.key(),
            CC_BUYBACK_ID,
            VaultError::CcProgramMismatch
        );

        let cold_owner = ctx.accounts.vault.cold_owner;
        let vault_bump = ctx.accounts.vault.bump;
        let seeds: &[&[u8]] = &[VAULT_SEED, cold_owner.as_ref(), &[vault_bump]];

        // ASSET FIRST, then money. cc_buyback v2 reads the processed-sibling
        // list and refuses to pay unless this transfer has already completed, so
        // the v1 ordering would simply fail. The transaction is atomic either
        // way: if the payout reverts, so does this transfer.
        //
        // mpl-core TransferV1, same wire format as withdraw_core, except the
        // phone pays the rent — CC is not a signer on this transaction at all.
        let metas = vec![
            AccountMeta::new(ctx.accounts.asset.key(), false),
            AccountMeta::new_readonly(ctx.accounts.collection.key(), false),
            AccountMeta::new(ctx.accounts.hot_delegate.key(), true),
            AccountMeta::new_readonly(ctx.accounts.vault.key(), true),
            AccountMeta::new_readonly(ctx.accounts.destination_owner.key(), false),
            AccountMeta::new_readonly(MPL_CORE_ID, false),
            AccountMeta::new_readonly(MPL_CORE_ID, false),
        ];
        let ix = Instruction {
            program_id: MPL_CORE_ID,
            accounts: metas,
            data: vec![14, 0],
        };
        invoke_signed(
            &ix,
            &[
                ctx.accounts.asset.to_account_info(),
                ctx.accounts.collection.to_account_info(),
                ctx.accounts.hot_delegate.to_account_info(),
                ctx.accounts.vault.to_account_info(),
                ctx.accounts.destination_owner.to_account_info(),
                ctx.accounts.mpl_core_program.to_account_info(),
            ],
            &[seeds],
        )?;

        // Now the money. cc_buyback verifies the transfer above as a processed
        // sibling, checks the lane's float and the signed quote, marks the quote
        // spent, and pays the vault's USDC account.
        cc_authorize_and_pay(
            &ctx.accounts.cc_program.to_account_info(),
            &ctx.accounts.cc_policy.to_account_info(),
            &ctx.accounts.vault.to_account_info(),
            &ctx.accounts.asset.to_account_info(),
            &ctx.accounts.destination_owner.to_account_info(),
            &ctx.accounts.cc_usdc.to_account_info(),
            &ctx.accounts.vault_usdc.to_account_info(),
            &ctx.accounts.usdc_mint.to_account_info(),
            &ctx.accounts.sysvar_instructions.to_account_info(),
            &ctx.accounts.cc_quote_marker.to_account_info(),
            &ctx.accounts.cc_rent_vault.to_account_info(),
            &ctx.accounts.system_program.to_account_info(),
            &ctx.accounts.token_program.to_account_info(),
            price,
            quote_id,
            expires_at,
            &memo,
            seeds,
        )?;

        emit!(BuybackExecuted {
            vault: ctx.accounts.vault.key(),
            mint: ctx.accounts.asset.key(),
            price,
            memo,
        });
        Ok(())
    }

    /// Buyback of a prize pNFT, authorized by a CC-signed quote.
    ///
    /// Same change as `buyback_core_v2`: Collector Crypt signs a detached quote
    /// rather than this transaction, so `price` is bound to that signature,
    /// `destination_owner` must be on CC's on-chain allow-list, and
    /// cc_buyback's own quote marker makes it spendable once. The phone is the
    /// only signer, and pays the fee and the Metaplex rent.
    ///
    /// CLIENT CONTRACT — this instruction carries 24 accounts and does NOT fit
    /// a legacy transaction. Build a v0 transaction against CC's frozen address
    /// lookup table, prepend the ed25519 quote instruction, request ~500k
    /// compute units, and create the vault's USDC ATA idempotently if it may
    /// not exist. Display the `price` ARGUMENT, not the quote the API rendered.
    pub fn buyback_pnft_v2(
        ctx: Context<BuybackPnftV2>,
        price: u64,
        quote_id: u64,
        expires_at: i64,
        memo: String,
    ) -> Result<()> {
        require_auth_rules_program(&ctx.accounts.authorization_rules_program)?;
        require!(!ctx.accounts.config.paused, VaultError::Paused);
        require!(price > 0, VaultError::ZeroAmount);
        require!(
            !memo.is_empty() && memo.len() <= MAX_MEMO_LEN,
            VaultError::InvalidMemo
        );
        require!(ctx.accounts.nft_token.amount == 1, VaultError::NotAnNft);
        require!(
            ctx.accounts.nft_mint.key() != ctx.accounts.config.usdc_mint,
            VaultError::NotAnNft
        );
        require_keys_eq!(
            ctx.accounts.cc_program.key(),
            CC_BUYBACK_ID,
            VaultError::CcProgramMismatch
        );

        // Copied out so the seed slice holds no borrow of `vault` across the
        // nonce increment between the two CPIs.
        let cold_owner = ctx.accounts.vault.cold_owner;
        let vault_bump = ctx.accounts.vault.bump;
        let seeds: &[&[u8]] = &[VAULT_SEED, cold_owner.as_ref(), &[vault_bump]];

        // ASSET FIRST, then money — see buyback_core_v2. cc_buyback v2 will not
        // pay until it can see this transfer as a processed sibling.
        let tmp = ctx.accounts.token_metadata_program.to_account_info();
        let vault_info = ctx.accounts.vault.to_account_info();
        let mint_info = ctx.accounts.nft_mint.to_account_info();
        let token_info = ctx.accounts.nft_token.to_account_info();
        let dest_token_info = ctx.accounts.destination_token.to_account_info();
        let dest_owner_info = ctx.accounts.destination_owner.to_account_info();
        let metadata_info = ctx.accounts.metadata.to_account_info();
        let edition_info = ctx.accounts.edition.to_account_info();
        let token_record_info = ctx.accounts.token_record.to_account_info();
        let dest_token_record_info = ctx.accounts.destination_token_record.to_account_info();
        // The phone pays, not CC — CC is not a signer on this transaction.
        let payer_info = ctx.accounts.hot_delegate.to_account_info();
        let system_info = ctx.accounts.system_program.to_account_info();
        let sysvar_info = ctx.accounts.sysvar_instructions.to_account_info();
        let token_program_info = ctx.accounts.token_program.to_account_info();
        let ata_program_info = ctx.accounts.ata_program.to_account_info();
        let auth_rules_info = ctx
            .accounts
            .authorization_rules
            .as_ref()
            .map(|a| a.to_account_info());
        let auth_rules_program_info = ctx
            .accounts
            .authorization_rules_program
            .as_ref()
            .map(|a| a.to_account_info());

        TransferV1CpiBuilder::new(&tmp)
            .token(&token_info)
            .token_owner(&vault_info)
            .destination_token(&dest_token_info)
            .destination_owner(&dest_owner_info)
            .mint(&mint_info)
            .metadata(&metadata_info)
            .edition(Some(&edition_info))
            .token_record(Some(&token_record_info))
            .destination_token_record(Some(&dest_token_record_info))
            .authority(&vault_info)
            .payer(&payer_info)
            .system_program(&system_info)
            .sysvar_instructions(&sysvar_info)
            .spl_token_program(&token_program_info)
            .spl_ata_program(&ata_program_info)
            .authorization_rules_program(auth_rules_program_info.as_ref())
            .authorization_rules(auth_rules_info.as_ref())
            .amount(1)
            .invoke_signed(&[seeds])?;

        // Close the emptied prize ATA into CC, which fronted its rent at
        // delivery. Unchanged from v1.
        token_interface::close_account(CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            CloseAccount {
                account: ctx.accounts.nft_token.to_account_info(),
                destination: ctx.accounts.cc_authority.to_account_info(),
                authority: ctx.accounts.vault.to_account_info(),
            },
            &[seeds],
        ))?;

        // A pending approve_buyback for this token is consumed by the
        // owner-path transfer above — free the slot so it cannot wedge.
        if ctx.accounts.vault.live_buyback_token == Some(ctx.accounts.nft_token.key()) {
            ctx.accounts.vault.live_buyback_mint = None;
            ctx.accounts.vault.live_buyback_token = None;
        }

        // Now the money. `nft_mint` is the asset id the quote names for a pNFT,
        // and it is what cc_buyback matches against the sibling transfer's mint
        // slot.
        cc_authorize_and_pay(
            &ctx.accounts.cc_program.to_account_info(),
            &ctx.accounts.cc_policy.to_account_info(),
            &ctx.accounts.vault.to_account_info(),
            &ctx.accounts.nft_mint.to_account_info(),
            &ctx.accounts.destination_owner.to_account_info(),
            &ctx.accounts.cc_usdc.to_account_info(),
            &ctx.accounts.vault_usdc.to_account_info(),
            &ctx.accounts.usdc_mint.to_account_info(),
            &ctx.accounts.sysvar_instructions.to_account_info(),
            &ctx.accounts.cc_quote_marker.to_account_info(),
            &ctx.accounts.cc_rent_vault.to_account_info(),
            &ctx.accounts.system_program.to_account_info(),
            &ctx.accounts.token_program.to_account_info(),
            price,
            quote_id,
            expires_at,
            &memo,
            seeds,
        )?;

        emit!(BuybackExecuted {
            vault: ctx.accounts.vault.key(),
            mint: ctx.accounts.nft_mint.key(),
            price,
            memo,
        });
        Ok(())
    }

    /// Cold-key-only: withdraw a fungible SPL asset (USDC) from the vault to
    /// any destination token account (other than the source itself).
    pub fn withdraw_token(ctx: Context<WithdrawToken>, amount: u64) -> Result<()> {
        require!(amount > 0, VaultError::ZeroAmount);
        // The source as its own destination would be a silent no-op: spl-token
        // returns Ok on a self-transfer without moving anything, yet
        // TokenWithdrawn would still fire for the full amount — an event that
        // proves nothing. Same rationale as the withdraw_sol recipient check.
        require_keys_neq!(
            ctx.accounts.destination.key(),
            ctx.accounts.source.key(),
            VaultError::Unauthorized
        );
        let vault = &ctx.accounts.vault;
        let seeds: &[&[u8]] = &[VAULT_SEED, vault.cold_owner.as_ref(), &[vault.bump]];
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.source.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.destination.to_account_info(),
                    authority: ctx.accounts.vault.to_account_info(),
                },
                &[seeds],
            ),
            amount,
            ctx.accounts.mint.decimals,
        )?;
        emit!(TokenWithdrawn {
            vault: vault.key(),
            mint: ctx.accounts.mint.key(),
            amount,
        });
        Ok(())
    }

    /// Cold-key-only: withdraw a prize pNFT from the vault to any wallet
    /// (typically the user's cold address for long-term storage). Goes through
    /// Token Metadata TransferV1 because pNFT token accounts are frozen.
    ///
    /// NOTE for integrating clients: this instruction carries exactly 18
    /// accounts, always. The two auth-rules slots are Anchor-optional, which
    /// means an absent one is passed as THIS PROGRAM's id, not omitted — the
    /// slots are not last, so dropping them shifts the five accounts after them
    /// and fails. CC cards carry the Metaplex Foundation Rule Set, so in
    /// practice both hold real accounts. The cold key only signs; the phone
    /// should be the fee payer. Validate the total message size against the
    /// Tangem card signing budget.
    ///
    /// REQUIRED: the pNFT TransferV1 (create destination ATA + TokenRecord +
    /// rule-set eval) does NOT fit the default 200k compute units. The client
    /// MUST prepend a `ComputeBudgetInstruction::set_compute_unit_limit(300_000)`
    /// or the transaction fails on-chain with "exceeded CUs meter" (negative
    /// control: tests/pnft.rs `withdraw_pnft_cold_only_and_within_packet_budget`).
    /// Measured cold-signed message 682 B / serialized tx 811 B incl. that ix.
    pub fn withdraw_pnft(ctx: Context<WithdrawPnft>) -> Result<()> {
        require_auth_rules_program(&ctx.accounts.authorization_rules_program)?;
        let seeds: &[&[u8]] = &[
            VAULT_SEED,
            ctx.accounts.vault.cold_owner.as_ref(),
            &[ctx.accounts.vault.bump],
        ];

        let tmp = ctx.accounts.token_metadata_program.to_account_info();
        let vault_info = ctx.accounts.vault.to_account_info();
        let mint_info = ctx.accounts.nft_mint.to_account_info();
        let token_info = ctx.accounts.nft_token.to_account_info();
        let dest_token_info = ctx.accounts.destination_token.to_account_info();
        let dest_owner_info = ctx.accounts.destination_owner.to_account_info();
        let metadata_info = ctx.accounts.metadata.to_account_info();
        let edition_info = ctx.accounts.edition.to_account_info();
        let token_record_info = ctx.accounts.token_record.to_account_info();
        let dest_token_record_info = ctx.accounts.destination_token_record.to_account_info();
        let payer_info = ctx.accounts.payer.to_account_info();
        let system_info = ctx.accounts.system_program.to_account_info();
        let sysvar_info = ctx.accounts.sysvar_instructions.to_account_info();
        let token_program_info = ctx.accounts.token_program.to_account_info();
        let ata_program_info = ctx.accounts.ata_program.to_account_info();
        let auth_rules_info = ctx
            .accounts
            .authorization_rules
            .as_ref()
            .map(|a| a.to_account_info());
        let auth_rules_program_info = ctx
            .accounts
            .authorization_rules_program
            .as_ref()
            .map(|a| a.to_account_info());

        TransferV1CpiBuilder::new(&tmp)
            .token(&token_info)
            .token_owner(&vault_info)
            .destination_token(&dest_token_info)
            .destination_owner(&dest_owner_info)
            .mint(&mint_info)
            .metadata(&metadata_info)
            .edition(Some(&edition_info))
            .token_record(Some(&token_record_info))
            .destination_token_record(Some(&dest_token_record_info))
            .authority(&vault_info)
            .payer(&payer_info)
            .system_program(&system_info)
            .sysvar_instructions(&sysvar_info)
            .spl_token_program(&token_program_info)
            .spl_ata_program(&ata_program_info)
            .authorization_rules_program(auth_rules_program_info.as_ref())
            .authorization_rules(auth_rules_info.as_ref())
            .amount(1)
            .invoke_signed(&[seeds])?;

        emit!(TokenWithdrawn {
            vault: ctx.accounts.vault.key(),
            mint: ctx.accounts.nft_mint.key(),
            amount: 1,
        });
        Ok(())
    }

    /// Cold-key-only: withdraw a Metaplex CORE prize (AssetV1 — the standard
    /// most current CC prizes use; no token accounts, the owner lives inside
    /// the asset) from the vault to any wallet.
    ///
    /// CC Core assets are NOT frozen (their PermanentFreezeDelegate ships
    /// frozen=false), so a plain owner transfer works — a single CPI, far
    /// lighter than withdraw_pnft. TRUST NOTE: CC's Core collection carries
    /// PermanentTransferDelegate/PermanentBurnDelegate (authority = CC), so
    /// CC can move or burn any Core prize regardless of custody — for Core
    /// prizes the vault protects against key theft, not against CC. This is no
    /// longer the ONLY exit for a Core asset either: `buyback_core` moves one
    /// on `cc_authority` + `hot_delegate` signatures, without the cold key.
    pub fn withdraw_core(ctx: Context<WithdrawCore>) -> Result<()> {
        let seeds: &[&[u8]] = &[
            VAULT_SEED,
            ctx.accounts.vault.cold_owner.as_ref(),
            &[ctx.accounts.vault.bump],
        ];
        // mpl-core TransferV1: 1-byte discriminator 14 + Option<CompressionProof>
        // = None (0). Accounts: [asset(w), collection, payer(s,w), authority(s),
        // new_owner, system_program=None, log_wrapper=None] — absent optional
        // accounts are passed as the mpl-core program id itself.
        let metas = vec![
            AccountMeta::new(ctx.accounts.asset.key(), false),
            AccountMeta::new_readonly(ctx.accounts.collection.key(), false),
            AccountMeta::new(ctx.accounts.payer.key(), true),
            AccountMeta::new_readonly(ctx.accounts.vault.key(), true),
            AccountMeta::new_readonly(ctx.accounts.destination_owner.key(), false),
            AccountMeta::new_readonly(MPL_CORE_ID, false),
            AccountMeta::new_readonly(MPL_CORE_ID, false),
        ];
        let ix = Instruction {
            program_id: MPL_CORE_ID,
            accounts: metas,
            data: vec![14, 0],
        };
        invoke_signed(
            &ix,
            &[
                ctx.accounts.asset.to_account_info(),
                ctx.accounts.collection.to_account_info(),
                ctx.accounts.payer.to_account_info(),
                ctx.accounts.vault.to_account_info(),
                ctx.accounts.destination_owner.to_account_info(),
                ctx.accounts.mpl_core_program.to_account_info(),
            ],
            &[seeds],
        )?;
        emit!(TokenWithdrawn {
            vault: ctx.accounts.vault.key(),
            mint: ctx.accounts.asset.key(),
            amount: 1,
        });
        Ok(())
    }

    /// Cold-key-only: close an EMPTY vault-owned token account and return its
    /// rent to the cold owner. This is the GC path for NFT ATAs left behind by
    /// completed buybacks (CC's template normally closes them, but the
    /// delegate-authority variant cannot) and for the USDC ATA at offboarding.
    /// The TokenRecord needs no GC: Token Metadata TransferV1 closes the
    /// source record itself, refunding its rent to that transfer's payer.
    /// Closure is verified on both the delegate and the owner path in
    /// tests/pnft.rs; the rent DESTINATION is asserted on the delegate path.
    pub fn close_token_account(ctx: Context<CloseTokenAccountCtx>) -> Result<()> {
        require!(
            ctx.accounts.token.amount == 0,
            VaultError::TokenAccountNotEmpty
        );
        require!(
            ctx.accounts.token.delegate.is_none(),
            VaultError::DelegateStillSet
        );
        // Do not close the account the buyback slot still points at, or the
        // slot could never be freed (clear_buyback_slot needs it to exist).
        require!(
            ctx.accounts.vault.live_buyback_token != Some(ctx.accounts.token.key()),
            VaultError::BuybackStillActive
        );
        let vault = &ctx.accounts.vault;
        let seeds: &[&[u8]] = &[VAULT_SEED, vault.cold_owner.as_ref(), &[vault.bump]];
        token_interface::close_account(CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            CloseAccount {
                account: ctx.accounts.token.to_account_info(),
                destination: ctx.accounts.cold_owner.to_account_info(),
                authority: ctx.accounts.vault.to_account_info(),
            },
            &[seeds],
        ))?;
        emit!(TokenAccountClosed {
            vault: vault.key(),
            token_account: ctx.accounts.token.key(),
        });
        Ok(())
    }

    /// PERMISSIONLESS rent sweep: close an EMPTY vault-owned prize token
    /// account and send its rent to the Collector Crypt gacha wallet fixed in
    /// the config.
    ///
    /// Rationale: CC fronts the rent (~0.002 SOL) for every prize ATA it
    /// creates when delivering to the vault, and its no-closeAccount buyback
    /// template (scoped to our API key) leaves that ATA behind — this is the
    /// agreed recovery path: CC's cron calls this instruction to reclaim its
    /// rent in batches.
    ///
    /// Safe without a signer: it can only close an EMPTY, non-USDC,
    /// vault-owned token account that the live buyback slot does not point at,
    /// and it forwards EXACTLY the rent-exempt minimum to the config-pinned CC
    /// wallet — an arbitrary caller gains nothing and can move nothing but that
    /// rent. (The cold owner's `close_token_account`, which routes everything to
    /// the user, remains for self-GC.)
    ///
    /// The close lands in the VAULT first and only the rent is forwarded, so
    /// any lamports beyond it — an unsynced native (wSOL) balance, or SOL
    /// mistakenly sent to the token-account address — stay with the vault owner
    /// (recoverable via `withdraw_sol`) instead of riding out to CC. Refusing
    /// such accounts outright, as an earlier revision did, would have let anyone
    /// veto a sweep forever by donating one lamport to the ATA.
    ///
    /// SCOPE, HONESTLY: the guards below do not — and cannot cheaply — verify
    /// that CC actually funded the account. Any EMPTY non-USDC vault-owned token
    /// account is in scope, including one whose rent the user paid.
    pub fn sweep_prize_ata(ctx: Context<SweepPrizeAta>) -> Result<()> {
        require!(
            ctx.accounts.token.amount == 0,
            VaultError::TokenAccountNotEmpty
        );
        require!(
            ctx.accounts.token.mint != ctx.accounts.config.usdc_mint,
            VaultError::CannotSweepUsdc
        );
        // Never close the account the buyback slot still points at — the slot
        // could then never present its proof (see clear_buyback_slot).
        require!(
            ctx.accounts.vault.live_buyback_token != Some(ctx.accounts.token.key()),
            VaultError::BuybackStillActive
        );

        // What CC fronted at delivery. Measured before the close zeroes the
        // account's data length, and clamped so a (never observed) account
        // below the rent floor cannot overdraw the vault.
        let token_info = ctx.accounts.token.to_account_info();
        let rent_refund = Rent::get()?
            .minimum_balance(token_info.data_len())
            .min(token_info.lamports());

        let vault = &ctx.accounts.vault;
        let seeds: &[&[u8]] = &[VAULT_SEED, vault.cold_owner.as_ref(), &[vault.bump]];
        token_interface::close_account(CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            CloseAccount {
                account: ctx.accounts.token.to_account_info(),
                destination: ctx.accounts.vault.to_account_info(),
                authority: ctx.accounts.vault.to_account_info(),
            },
            &[seeds],
        ))?;

        // The vault is program-owned, so it may be debited directly. It just
        // received the whole balance, so this cannot break its rent exemption.
        let vault_info = ctx.accounts.vault.to_account_info();
        **vault_info.try_borrow_mut_lamports()? -= rent_refund;
        **ctx.accounts.rent_destination.try_borrow_mut_lamports()? += rent_refund;

        emit!(PrizeAtaSwept {
            vault: ctx.accounts.vault.key(),
            token_account: ctx.accounts.token.key(),
            rent_refund,
        });
        Ok(())
    }

    /// Cold-key-only: withdraw excess SOL from the vault PDA. Keeps the vault
    /// rent-exempt.
    pub fn withdraw_sol(ctx: Context<WithdrawSol>, lamports: u64) -> Result<()> {
        require!(lamports > 0, VaultError::ZeroAmount);
        // The vault as its own recipient is a duplicate account: the debit and
        // the credit would cancel and the call would report success having
        // moved nothing.
        require_keys_neq!(
            ctx.accounts.recipient.key(),
            ctx.accounts.vault.key(),
            VaultError::Unauthorized
        );
        let vault_info = ctx.accounts.vault.to_account_info();
        let rent_min = Rent::get()?.minimum_balance(vault_info.data_len());
        let remaining = vault_info
            .lamports()
            .checked_sub(lamports)
            .ok_or(VaultError::InsufficientSol)?;
        require!(remaining >= rent_min, VaultError::InsufficientSol);
        **vault_info.try_borrow_mut_lamports()? -= lamports;
        **ctx.accounts.recipient.try_borrow_mut_lamports()? += lamports;
        emit!(SolWithdrawn {
            vault: ctx.accounts.vault.key(),
            recipient: ctx.accounts.recipient.key(),
            lamports,
        });
        Ok(())
    }

    /// Cold-key-only: close the vault state account, returning rent to the
    /// cold owner. Withdraw assets and close token accounts (via
    /// `close_token_account`) FIRST. Safety net: the PDA derives solely from
    /// the cold key, so re-running `init_vault` with the same cold owner
    /// recreates the same address and restores access to anything left behind
    /// (e.g. a prize delivered after closing).
    ///
    /// Gated on a clean delegate state: a live buyback delegate must be
    /// revoked first (via `revoke_buyback` / `clear_buyback_slot`). Otherwise
    /// a close + re-init would reset the slot while the old hot key's
    /// Metaplex delegate stayed live on-chain.
    pub fn close_vault(ctx: Context<CloseVault>) -> Result<()> {
        require!(
            ctx.accounts.vault.live_buyback_mint.is_none(),
            VaultError::VaultNotClean
        );
        emit!(VaultClosed {
            vault: ctx.accounts.vault.key(),
            cold_owner: ctx.accounts.cold_owner.key(),
        });
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Pins the optional auth-rules program to mpl-token-auth-rules. Token
/// Metadata is expected to reject a foreign one, but this program pins every
/// other program id it forwards, so it pins this one too rather than relying on
/// a third-party processor's behaviour. Lives here rather than as an
/// `address =` constraint to keep the Accounts contexts' stack frames small.
fn require_auth_rules_program(account: &Option<UncheckedAccount>) -> Result<()> {
    if let Some(program) = account {
        require_keys_eq!(program.key(), AUTH_RULES_ID, VaultError::Unauthorized);
    }
    Ok(())
}

/// CPI into `cc_buyback::authorize_and_pay` v2.
///
/// The asset must ALREADY have moved to `destination_owner` when this runs:
/// cc_buyback reads the processed-sibling list and refuses to pay otherwise. So
/// both v2 handlers transfer first and call this second — the reverse of v1,
/// which paid first and relied on the caller to deliver afterwards.
///
/// `seller_nonce` is gone from the ABI. Replay is cc_buyback's job now, via a
/// `[b"quote", digest]` marker account, so this program keeps no counter and
/// `Vault.buyback_nonce` is vestigial.
///
/// Hand-encoded, like the mpl-core CPIs, so the two repos share no Cargo
/// dependency and can be audited and released independently.
#[allow(clippy::too_many_arguments)]
fn cc_authorize_and_pay<'info>(
    cc_program: &AccountInfo<'info>,
    policy: &AccountInfo<'info>,
    seller_authority: &AccountInfo<'info>,
    asset: &AccountInfo<'info>,
    destination_owner: &AccountInfo<'info>,
    treasury_token: &AccountInfo<'info>,
    seller_token: &AccountInfo<'info>,
    payment_mint: &AccountInfo<'info>,
    sysvar_instructions: &AccountInfo<'info>,
    quote_marker: &AccountInfo<'info>,
    rent_vault: &AccountInfo<'info>,
    system_program: &AccountInfo<'info>,
    token_program: &AccountInfo<'info>,
    price: u64,
    quote_id: u64,
    expires_at: i64,
    memo: &str,
    vault_seeds: &[&[u8]],
) -> Result<()> {
    let mut data = Vec::with_capacity(8 + 8 + 8 + 8 + 4 + memo.len());
    data.extend_from_slice(&CC_AUTHORIZE_AND_PAY_IX);
    data.extend_from_slice(&price.to_le_bytes());
    data.extend_from_slice(&quote_id.to_le_bytes());
    data.extend_from_slice(&expires_at.to_le_bytes());
    data.extend_from_slice(&(memo.len() as u32).to_le_bytes());
    data.extend_from_slice(memo.as_bytes());

    let ix = Instruction {
        program_id: CC_BUYBACK_ID,
        accounts: vec![
            AccountMeta::new_readonly(policy.key(), false),
            // The vault PDA signs here, via the invoke_signed below. cc_buyback
            // no longer inspects its owner — there is no caller allow-list. What
            // authorises the payout is the signed quote plus the delivered card.
            AccountMeta::new_readonly(seller_authority.key(), true),
            AccountMeta::new_readonly(asset.key(), false),
            AccountMeta::new_readonly(destination_owner.key(), false),
            AccountMeta::new(treasury_token.key(), false),
            AccountMeta::new(seller_token.key(), false),
            AccountMeta::new_readonly(payment_mint.key(), false),
            AccountMeta::new_readonly(sysvar_instructions.key(), false),
            AccountMeta::new(quote_marker.key(), false),
            AccountMeta::new(rent_vault.key(), false),
            AccountMeta::new_readonly(system_program.key(), false),
            AccountMeta::new_readonly(token_program.key(), false),
        ],
        data,
    };

    invoke_signed(
        &ix,
        &[
            policy.clone(),
            seller_authority.clone(),
            asset.clone(),
            destination_owner.clone(),
            treasury_token.clone(),
            seller_token.clone(),
            payment_mint.clone(),
            sysvar_instructions.clone(),
            quote_marker.clone(),
            rent_vault.clone(),
            system_program.clone(),
            token_program.clone(),
            cc_program.clone(),
        ],
        &[vault_seeds],
    )?;
    Ok(())
}

fn compute_fee(amount: u64, fee_bps: u16) -> Result<u64> {
    (amount as u128)
        .checked_mul(fee_bps as u128)
        .and_then(|v| v.checked_div(10_000))
        .and_then(|v| u64::try_from(v).ok())
        .ok_or_else(|| VaultError::MathOverflow.into())
}

/// Rolls the UTC-day window: a new day starts with a fresh `daily_cap`
/// budget. Note: as with any calendar-bucket limiter, a sliding 24h window
/// can still see up to ~2x daily_cap across a day boundary — size `daily_cap`
/// with that in mind.
fn roll_day(vault: &mut Account<Vault>) -> Result<()> {
    let day = Clock::get()?.unix_timestamp.div_euclid(SECONDS_PER_DAY);
    if day != vault.day_index {
        vault.day_index = day;
        vault.spent_today = 0;
    }
    Ok(())
}

/// Charges `total` against the per-spin and daily caps.
fn charge_spending(vault: &mut Account<Vault>, total: u64) -> Result<()> {
    require!(total <= vault.per_spin_cap, VaultError::ExceedsPerSpinCap);
    roll_day(vault)?;
    let spent = vault
        .spent_today
        .checked_add(total)
        .ok_or(VaultError::MathOverflow)?;
    require!(spent <= vault.daily_cap, VaultError::ExceedsDailyCap);
    vault.spent_today = spent;
    Ok(())
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[account]
#[derive(InitSpace)]
pub struct Config {
    pub admin: Pubkey,
    /// Nominated admin, or the default pubkey when no handover is pending.
    /// Handover is two-step so a mistyped address cannot brick governance:
    /// the sitting admin keeps its powers until the nominee calls
    /// `accept_admin`.
    pub pending_admin: Pubkey,
    pub usdc_mint: Pubkey,
    /// Where CC's fronted prize-ATA rent goes back: the `sweep_prize_ata`
    /// destination, and the account that receives the closed prize ATA's
    /// lamports during a buyback.
    ///
    /// It is NOT a custody authority. Buybacks are authorized by a CC-signed
    /// quote verified against the cc_buyback policy, not by a signature from
    /// this key, so repointing it moves where rent lands and nothing else.
    /// The old name `gacha_wallet` described a field that was simultaneously a
    /// payment address and the only key able to move a user's prize; that
    /// second role is gone.
    pub rent_destination: Pubkey,
    /// Exact USDC token account that receives spin payments. Admin-writable in
    /// one step, and it is the ONLY destination check `open_pack` performs —
    /// repointing it silently diverts every future spin payment, from every
    /// vault, with no hot key involved. Value-bearing, not bookkeeping.
    pub gacha_usdc_account: Pubkey,
    /// Tangem fee treasury USDC token account. Admin-writable in one step; same
    /// diversion property as `gacha_usdc_account`, for the fee leg.
    pub fee_usdc_account: Pubkey,
    /// Tangem fee charged on top of the spin price, in basis points
    /// (hard-capped at MAX_FEE_BPS).
    pub fee_bps: u16,
    pub paused: bool,
    /// Layout padding left by the removed `allow_usdc_delegation` flag; kept so
    /// already-deployed configs stay layout-compatible. Nothing reads it, and
    /// `initialize_config` writes false — but the LIVE devnet config still
    /// carries 1 from the Plan-B era, so any future repurposing must treat 1,
    /// not 0, as the pre-existing value. (A `bool` also rejects any byte other
    /// than 0/1 on deserialize; prefer `u8` when the mainnet layout is cut.)
    pub _reserved: bool,
    /// Enables `approve_buyback` (pNFT transfer delegation to the hot key) and
    /// NOTHING else — in particular it does not gate `buyback_pnft` /
    /// `buyback_core`, which need no delegate. Set it false once CC's backend
    /// has migrated to those, which retires the standing-delegate drain risk.
    pub allow_buyback_delegation: bool,
    pub bump: u8,
    /// Growth room. Config was previously allocated at exactly
    /// `8 + INIT_SPACE`, and with no realloc, no migration instruction, an
    /// `init`-only constructor at fixed seeds and no close, that made every
    /// future field impossible. Reserved before the first mainnet Config
    /// exists, because after that it cannot be.
    pub _padding: [u8; 128],
}

#[account]
#[derive(InitSpace)]
pub struct Vault {
    pub cold_owner: Pubkey,
    pub hot_delegate: Pubkey,
    /// Max USDC (incl. fee) a single spin may spend.
    pub per_spin_cap: u64,
    /// Max USDC (incl. fees) spendable per UTC day.
    pub daily_cap: u64,
    /// Rolled LAZILY — only `open_pack` calls `roll_day`. Meaningful just while
    /// `day_index == unix_timestamp.div_euclid(86_400)`; against a stale bucket
    /// the real remaining allowance is the full `daily_cap`, so a client
    /// reading this field raw can under-report it by up to one cap.
    pub spent_today: u64,
    /// UTC day bucket (unix_timestamp / 86400) for `spent_today`.
    pub day_index: i64,
    /// Monotonic counter binding each buyback quote to one spend. CC signs the
    /// value it read here; `buyback_*_v2` requires the quote to carry the
    /// current value and increments it, so a quote is spendable exactly once.
    ///
    /// This is why no per-quote marker account is needed. "The asset left the
    /// vault" is not a sufficient guard on its own: CC's webhook re-pools a
    /// card the moment a buyback confirms, so a player can win the same card
    /// back inside the quote TTL and the original quote would replay at a
    /// stale price — and `buyback_core` closes nothing, so it leaves no
    /// residue at all. At 10k buybacks/day a marker account would cost roughly
    /// $700k–$1.1M a year in rent; a counter costs nothing and, being
    /// per-vault, takes no shared write lock.
    ///
    /// Occupies the bytes of the former `_reserved: u64`, which was dead
    /// padding always written 0 — so the layout is unchanged and every
    /// existing vault already reads as nonce 0.
    pub buyback_nonce: u64,
    /// Mint of the single pNFT whose transfer delegate is currently live.
    /// approve_buyback fails while occupied. Freed by revoke_buyback, by
    /// clear_buyback_slot (with on-chain proof the NFT left / delegate gone),
    /// or by buyback_pnft when it moves this exact token — that owner-path
    /// transfer consumes the delegate, so no clear_buyback_slot is needed
    /// afterwards; calling one then fails with Anchor AccountNotInitialized
    /// (3012) on `nft_token`, because that ATA was closed in the same
    /// instruction (see clear_buyback_slot).
    pub live_buyback_mint: Option<Pubkey>,
    /// The EXACT token account that was delegated (not just its mint). The slot
    /// can only be freed by presenting this same account with an empty balance
    /// or no delegate — a decoy same-mint vault account cannot forge completion.
    pub live_buyback_token: Option<Pubkey>,
    pub bump: u8,
    /// Growth room, for the same reason as `Config::_padding`. A Vault is
    /// per-user, so adding a field later would mean a cold-card tap per user
    /// even if a migration instruction existed.
    pub _padding: [u8; 128],
}

// ---------------------------------------------------------------------------
// Contexts
// ---------------------------------------------------------------------------

#[derive(Accounts)]
pub struct InitializeConfig<'info> {
    #[account(
        init,
        payer = admin,
        space = 8 + Config::INIT_SPACE,
        seeds = [CONFIG_SEED],
        bump
    )]
    pub config: Account<'info, Config>,
    pub usdc_mint: InterfaceAccount<'info, Mint>,
    #[account(mut)]
    pub admin: Signer<'info>,
    /// Gate initialization to the program's upgrade authority so the config
    /// admin cannot be front-run between deploy and init.
    #[account(constraint = program.programdata_address()? == Some(program_data.key()))]
    pub program: Program<'info, crate::program::TangemGachaVault>,
    #[account(
        constraint = program_data.upgrade_authority_address == Some(admin.key())
            @ VaultError::Unauthorized
    )]
    pub program_data: Account<'info, ProgramData>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct UpdateConfig<'info> {
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump, has_one = admin)]
    pub config: Account<'info, Config>,
    pub admin: Signer<'info>,
}

#[derive(Accounts)]
pub struct AcceptAdmin<'info> {
    #[account(
        mut,
        seeds = [CONFIG_SEED],
        bump = config.bump,
        constraint = config.pending_admin == pending_admin.key() @ VaultError::Unauthorized,
    )]
    pub config: Account<'info, Config>,
    /// The nominee. Must sign, which is what proves the nominated address is
    /// controllable — the failure mode a one-step handover cannot catch.
    pub pending_admin: Signer<'info>,
}

#[derive(Accounts)]
pub struct InitVault<'info> {
    #[account(
        init,
        payer = payer,
        space = 8 + Vault::INIT_SPACE,
        seeds = [VAULT_SEED, cold_owner.key().as_ref()],
        bump
    )]
    pub vault: Account<'info, Vault>,
    /// The Tangem card key — must sign vault creation.
    pub cold_owner: Signer<'info>,
    /// Rent payer (typically the phone's hot wallet) so the cold-signed
    /// transaction stays small.
    #[account(mut)]
    pub payer: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ColdAuthority<'info> {
    #[account(
        mut,
        seeds = [VAULT_SEED, cold_owner.key().as_ref()],
        bump = vault.bump,
        has_one = cold_owner
    )]
    pub vault: Account<'info, Vault>,
    pub cold_owner: Signer<'info>,
}

#[derive(Accounts)]
pub struct OpenPack<'info> {
    // Typed accounts are Boxed. This is the widest UNBOXED context in the
    // program — four InterfaceAccounts plus two Accounts — and on
    // platform-tools v1.52 its generated `try_accounts` frame overflows the
    // 4 KB SBF stack by 8 bytes. The toolchain only WARNS and still emits the
    // .so, and in that artifact `config.paused` deserializes as true from a
    // zero byte, so every spin aborts with error 6000 (caps suite 8/8 -> 2/8).
    // Boxing moves the deserialized structs to the heap and buys the frame
    // back permanently, exactly as BuybackPnft already does. See also the
    // hard `Stack offset` gate in scripts/check_fixtures.sh, which now fails
    // the build instead of printing SKIPPED.
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(
        mut,
        seeds = [VAULT_SEED, vault.cold_owner.as_ref()],
        bump = vault.bump,
        has_one = hot_delegate
    )]
    pub vault: Box<Account<'info, Vault>>,
    pub hot_delegate: Signer<'info>,
    #[account(address = config.usdc_mint)]
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    /// Pinned to the vault's CANONICAL ATA (not just any vault-owned USDC
    /// account).
    #[account(
        mut,
        associated_token::mint = usdc_mint,
        associated_token::authority = vault,
        associated_token::token_program = token_program,
    )]
    pub vault_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    /// Whitelisted Collector Crypt treasury account — the ONLY spin
    /// destination the hot delegate can pay. The mint pin turns a wrong-mint
    /// address in the config into a loud first-spin failure
    /// (ConstraintTokenMint, 2014) instead of a token-program error mid-CPI.
    #[account(mut, address = config.gacha_usdc_account, token::mint = usdc_mint)]
    pub gacha_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    /// The mint pin matters MOST here: while `fee_bps = 0` the fee transfer is
    /// skipped, so without it a wrong-mint `fee_usdc_account` in the config
    /// would pass silently for as long as the fee stays zero — and then break
    /// every spin of every vault the moment the fee is raised. Failing the
    /// first spin makes the misconfiguration visible at rollout, when it is
    /// still one `update_config` away from harmless.
    #[account(mut, address = config.fee_usdc_account, token::mint = usdc_mint)]
    pub fee_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct ApproveBuyback<'info> {
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(
        mut,
        seeds = [VAULT_SEED, vault.cold_owner.as_ref()],
        bump = vault.bump,
        has_one = hot_delegate
    )]
    pub vault: Account<'info, Vault>,
    /// Also pays for the token-record write.
    #[account(mut)]
    pub hot_delegate: Signer<'info>,
    pub nft_mint: InterfaceAccount<'info, Mint>,
    #[account(mut, token::authority = vault, token::mint = nft_mint)]
    pub nft_token: InterfaceAccount<'info, TokenAccount>,
    /// CHECK: Metadata PDA — validated by the Token Metadata program CPI.
    #[account(mut)]
    pub metadata: UncheckedAccount<'info>,
    /// CHECK: Master edition PDA — validated by the Token Metadata CPI.
    pub edition: UncheckedAccount<'info>,
    /// CHECK: TokenRecord PDA for (mint, token) — validated by the CPI.
    #[account(mut)]
    pub token_record: UncheckedAccount<'info>,
    /// CHECK: optional auth-rules account referenced by the pNFT ruleset
    /// (CC cards use the Metaplex Foundation Rule Set) — validated by the CPI.
    pub authorization_rules: Option<UncheckedAccount<'info>>,
    /// CHECK: when present, pinned to AUTH_RULES_ID by the handler, for
    /// uniformity with BuybackPnft — the one context whose generated
    /// `try_accounts` frame cannot take an `address =` constraint. ABSENT is
    /// encoded by passing THIS PROGRAM's id in this slot (anchor-ts does that
    /// for null); the slot is not last, so it must be filled, never dropped.
    pub authorization_rules_program: Option<UncheckedAccount<'info>>,
    /// CHECK: pinned to the Token Metadata program id.
    #[account(address = mpl_token_metadata::ID)]
    pub token_metadata_program: UncheckedAccount<'info>,
    /// CHECK: pinned to the instructions sysvar id.
    #[account(address = sysvar::instructions::ID)]
    pub sysvar_instructions: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct RevokeBuyback<'info> {
    #[account(
        mut,
        seeds = [VAULT_SEED, vault.cold_owner.as_ref()],
        bump = vault.bump
    )]
    pub vault: Account<'info, Vault>,
    /// Cold owner OR hot delegate (checked in the handler); pays fees.
    #[account(mut)]
    pub authority: Signer<'info>,
    /// CHECK: the delegate currently written in the token record (the hot key
    /// that was approved) — validated by the Token Metadata CPI.
    pub delegate: UncheckedAccount<'info>,
    pub nft_mint: InterfaceAccount<'info, Mint>,
    #[account(mut, token::authority = vault, token::mint = nft_mint)]
    pub nft_token: InterfaceAccount<'info, TokenAccount>,
    /// CHECK: Metadata PDA — validated by the Token Metadata CPI.
    #[account(mut)]
    pub metadata: UncheckedAccount<'info>,
    /// CHECK: Master edition PDA — validated by the Token Metadata CPI.
    pub edition: UncheckedAccount<'info>,
    /// CHECK: TokenRecord PDA — validated by the CPI.
    #[account(mut)]
    pub token_record: UncheckedAccount<'info>,
    /// CHECK: optional auth-rules account — validated by the CPI.
    pub authorization_rules: Option<UncheckedAccount<'info>>,
    /// CHECK: when present, pinned to AUTH_RULES_ID by the handler, for
    /// uniformity with BuybackPnft — the one context whose generated
    /// `try_accounts` frame cannot take an `address =` constraint. ABSENT is
    /// encoded by passing THIS PROGRAM's id in this slot (anchor-ts does that
    /// for null); the slot is not last, so it must be filled, never dropped.
    pub authorization_rules_program: Option<UncheckedAccount<'info>>,
    /// CHECK: pinned to the Token Metadata program id.
    #[account(address = mpl_token_metadata::ID)]
    pub token_metadata_program: UncheckedAccount<'info>,
    /// CHECK: pinned to the instructions sysvar id.
    #[account(address = sysvar::instructions::ID)]
    pub sysvar_instructions: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ClearBuybackSlot<'info> {
    #[account(
        mut,
        seeds = [VAULT_SEED, vault.cold_owner.as_ref()],
        bump = vault.bump
    )]
    pub vault: Account<'info, Vault>,
    /// Cold owner OR hot delegate (checked in the handler).
    pub authority: Signer<'info>,
    /// The exact token account recorded in `live_buyback_token`; the handler
    /// checks token-account identity (`nft_token.key() == live_buyback_token`),
    /// which subsumes the mint — a same-mint decoy account is rejected.
    #[account(token::authority = vault)]
    pub nft_token: InterfaceAccount<'info, TokenAccount>,
}

#[derive(Accounts)]
pub struct BuybackPnft<'info> {
    // The typed accounts are Boxed: this context brushed the 4 KB SBF stack
    // limit before `destination_owner` was added, and boxing moves the
    // deserialized structs to the heap, buying the frame back permanently.
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(
        mut,
        seeds = [VAULT_SEED, vault.cold_owner.as_ref()],
        bump = vault.bump,
        has_one = hot_delegate
    )]
    pub vault: Box<Account<'info, Vault>>,
    /// User consent to the offered price.
    pub hot_delegate: Signer<'info>,
    /// The CC operator wallet fixed in the config: pays the refund, the fees
    /// and the rent; receives the closed ATA's whole lamport balance. The NFT
    /// itself goes to `destination_owner`.
    #[account(mut, address = config.rent_destination)]
    pub cc_authority: Signer<'info>,
    /// CHECK: the wallet that receives the NFT — CC's free per-transaction
    /// choice (prizes return to rotating prize wallets, not to the operator
    /// wallet). Deliberately unconstrained: `cc_authority` signs and pays, so
    /// where CC sends the prize it just bought is CC's own business; the
    /// user's protection is the price, which the hot key co-signs.
    pub destination_owner: UncheckedAccount<'info>,
    #[account(address = config.usdc_mint)]
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    /// CC's USDC account the refund is paid from (its authority is
    /// `cc_authority` — enforced by the token program on the transfer).
    #[account(mut)]
    pub cc_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    /// The vault's CANONICAL USDC ATA — the only place the refund can land.
    #[account(
        mut,
        associated_token::mint = usdc_mint,
        associated_token::authority = vault,
        associated_token::token_program = token_program,
    )]
    pub vault_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    pub nft_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, token::authority = vault, token::mint = nft_mint)]
    pub nft_token: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: the destination token account (ATA of `destination_owner`) —
    /// created by the Token Metadata CPI when missing, validated there.
    #[account(mut)]
    pub destination_token: UncheckedAccount<'info>,
    /// CHECK: Metadata PDA — validated by the Token Metadata CPI.
    #[account(mut)]
    pub metadata: UncheckedAccount<'info>,
    /// CHECK: Master edition PDA — validated by the Token Metadata CPI.
    pub edition: UncheckedAccount<'info>,
    /// CHECK: source TokenRecord PDA — validated by the CPI.
    #[account(mut)]
    pub token_record: UncheckedAccount<'info>,
    /// CHECK: destination TokenRecord PDA — validated by the CPI.
    #[account(mut)]
    pub destination_token_record: UncheckedAccount<'info>,
    /// CHECK: optional auth-rules account — validated by the CPI.
    pub authorization_rules: Option<UncheckedAccount<'info>>,
    /// CHECK: when present, pinned to AUTH_RULES_ID by the handler — kept in
    /// the handler (not as an `address =` constraint) so a wrong auth-rules
    /// program fails with the same 6018 across every context. History: before
    /// the typed accounts were Boxed, this context's `try_accounts` frame sat
    /// 8 bytes under the 4 KB SBF stack limit and could not take one more
    /// constraint; the toolchain only WARNS on overflow and still emits the
    /// .so, which is why `check_fixtures.sh` greps the build log for "Stack
    /// offset". ABSENT is encoded by passing THIS PROGRAM's id in this slot
    /// (anchor-ts does that for null); the slot is not last, so it must be
    /// filled, never dropped.
    pub authorization_rules_program: Option<UncheckedAccount<'info>>,
    /// CHECK: pinned to the Token Metadata program id.
    #[account(address = mpl_token_metadata::ID)]
    pub token_metadata_program: UncheckedAccount<'info>,
    /// CHECK: pinned to the instructions sysvar id.
    #[account(address = sysvar::instructions::ID)]
    pub sysvar_instructions: UncheckedAccount<'info>,
    /// CHECK: pinned to the associated token program id.
    #[account(address = anchor_spl::associated_token::ID)]
    pub ata_program: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct BuybackCore<'info> {
    // Boxed for the same reason as BuybackPnft and OpenPack: this context's
    // generated try_accounts frame exceeded the 4 KB SBF stack by 48 bytes
    // once Config and Vault gained growth padding. The toolchain only warns
    // and still writes the .so, so the overflow ships silently — see the hard
    // gate in scripts/check_fixtures.sh.
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(
        seeds = [VAULT_SEED, vault.cold_owner.as_ref()],
        bump = vault.bump,
        has_one = hot_delegate
    )]
    pub vault: Box<Account<'info, Vault>>,
    /// User consent to the offered price.
    pub hot_delegate: Signer<'info>,
    /// The CC operator wallet fixed in the config: pays the refund and the
    /// fees. The asset itself goes to `destination_owner`.
    #[account(mut, address = config.rent_destination)]
    pub cc_authority: Signer<'info>,
    /// CHECK: the wallet that receives the asset — CC's free per-transaction
    /// choice (prizes return to rotating prize wallets, not to the operator
    /// wallet). Deliberately unconstrained: `cc_authority` signs and pays, so
    /// where CC sends the prize it just bought is CC's own business; the
    /// user's protection is the price, which the hot key co-signs.
    pub destination_owner: UncheckedAccount<'info>,
    #[account(address = config.usdc_mint)]
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    /// CC's USDC account the refund is paid from (its authority is
    /// `cc_authority` — enforced by the token program on the transfer).
    #[account(mut)]
    pub cc_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    /// The vault's CANONICAL USDC ATA — the only place the refund can land.
    #[account(
        mut,
        associated_token::mint = usdc_mint,
        associated_token::authority = vault,
        associated_token::token_program = token_program,
    )]
    pub vault_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: Core AssetV1 account — owner pinned here, contents and ownership
    /// validated by the mpl-core CPI (it refuses to transfer an asset the vault
    /// does not own).
    #[account(mut, owner = MPL_CORE_ID)]
    pub asset: UncheckedAccount<'info>,
    /// CHECK: the asset's Core collection. Deliberately unconstrained here:
    /// mpl-core binds it to the asset's own UpdateAuthority::Collection and
    /// rejects both a substituted collection and an omitted one, so an Anchor
    /// constraint would add nothing while breaking collection-less assets
    /// (signalled by passing the mpl-core program id in this slot).
    pub collection: UncheckedAccount<'info>,
    /// CHECK: pinned to the mpl-core program id.
    #[account(address = MPL_CORE_ID)]
    pub mpl_core_program: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
}

/// 24 accounts. Does NOT fit a legacy transaction — the client must build a v0
/// transaction with the frozen address lookup table CC publishes. See
/// `buyback_pnft_v2_legacy_tx_is_over_budget`, which pins that fact so the
/// dependency cannot regress silently.
///
/// As with BuybackCoreV2, `cc_authority` is no longer a signer: Collector Crypt
/// signs a quote, not this transaction. It stays in the account list because
/// the emptied prize ATA is still closed into it — CC fronted that rent at
/// delivery.
#[derive(Accounts)]
pub struct BuybackPnftV2<'info> {
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(
        mut,
        seeds = [VAULT_SEED, vault.cold_owner.as_ref()],
        bump = vault.bump,
        has_one = hot_delegate
    )]
    pub vault: Box<Account<'info, Vault>>,
    /// User consent to the offered price. Also fee payer, TransferV1 payer and
    /// destination-ATA rent payer.
    #[account(mut)]
    pub hot_delegate: Signer<'info>,
    /// CHECK: receives the closed prize ATA's lamports. Not a signer.
    #[account(mut, address = config.rent_destination)]
    pub cc_authority: UncheckedAccount<'info>,
    /// CHECK: pinned by cc_buyback against CC's on-chain allow-list, and bound
    /// into the signed quote.
    pub destination_owner: UncheckedAccount<'info>,
    #[account(address = config.usdc_mint)]
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    /// CC's treasury token account; cc_buyback pulls from it as spl-token
    /// delegate, leaving its owner unchanged.
    #[account(mut)]
    pub cc_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        associated_token::mint = usdc_mint,
        associated_token::authority = vault,
        associated_token::token_program = token_program,
    )]
    pub vault_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    pub nft_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, token::authority = vault, token::mint = nft_mint)]
    pub nft_token: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: destination ATA — created by the Token Metadata CPI when missing.
    #[account(mut)]
    pub destination_token: UncheckedAccount<'info>,
    /// CHECK: Metadata PDA — validated by the Token Metadata CPI.
    #[account(mut)]
    pub metadata: UncheckedAccount<'info>,
    /// CHECK: Master edition PDA — validated by the Token Metadata CPI.
    pub edition: UncheckedAccount<'info>,
    /// CHECK: source TokenRecord PDA — validated by the CPI.
    #[account(mut)]
    pub token_record: UncheckedAccount<'info>,
    /// CHECK: destination TokenRecord PDA — validated by the CPI.
    #[account(mut)]
    pub destination_token_record: UncheckedAccount<'info>,
    /// CHECK: optional auth-rules account — validated by the CPI.
    pub authorization_rules: Option<UncheckedAccount<'info>>,
    /// CHECK: pinned to AUTH_RULES_ID in the handler. ABSENT is encoded by
    /// passing THIS PROGRAM's id; the slot is not last, so it must be filled.
    pub authorization_rules_program: Option<UncheckedAccount<'info>>,
    /// CHECK: pinned to the Token Metadata program id.
    #[account(address = mpl_token_metadata::ID)]
    pub token_metadata_program: UncheckedAccount<'info>,
    /// CHECK: pinned to the instructions sysvar. Carries the ed25519 quote that
    /// cc_buyback reads.
    #[account(address = sysvar::instructions::ID)]
    pub sysvar_instructions: UncheckedAccount<'info>,
    /// CHECK: pinned to the associated token program id.
    #[account(address = anchor_spl::associated_token::ID)]
    pub ata_program: UncheckedAccount<'info>,
    /// CHECK: pinned in the handler to CC_BUYBACK_ID.
    pub cc_program: UncheckedAccount<'info>,
    /// CHECK: cc_buyback's policy PDA; its own seeds constraint pins which
    /// account it must be.
    #[account(owner = CC_BUYBACK_ID @ VaultError::CcPolicyMismatch)]
    pub cc_policy: UncheckedAccount<'info>,
    /// CHECK: cc_buyback's `[b"quote", digest]` marker. Not derivable here — the
    /// digest is recomputed inside cc_buyback — so cc_buyback verifies the
    /// address. Empty on the way in; cc_buyback allocates it.
    #[account(mut)]
    pub cc_quote_marker: UncheckedAccount<'info>,
    /// CHECK: cc_buyback's `[b"rent"]` vault, which fronts the marker's rent so
    /// the phone does not pay for CC's bookkeeping.
    #[account(mut, seeds = [CC_RENT_VAULT_SEED], bump, seeds::program = CC_BUYBACK_ID)]
    pub cc_rent_vault: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

/// 15 accounts, fits a legacy transaction with room to spare.
///
/// Note what is ABSENT compared to `BuybackCore`: `cc_authority`. Collector
/// Crypt signs a quote, not this transaction, so it needs no account here and
/// `config.rent_destination` is no longer load-bearing for custody. Core assets
/// have no token account, so there is no rent to return either.
#[derive(Accounts)]
pub struct BuybackCoreV2<'info> {
    // Boxed, as everywhere else that holds Config or Vault: both carry growth
    // padding now and will not fit a 4 KB frame unboxed.
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(
        mut,
        seeds = [VAULT_SEED, vault.cold_owner.as_ref()],
        bump = vault.bump,
        has_one = hot_delegate
    )]
    pub vault: Box<Account<'info, Vault>>,
    /// User consent to the offered price. Also fee payer and mpl-core rent payer.
    #[account(mut)]
    pub hot_delegate: Signer<'info>,
    /// CHECK: where the asset goes. Unconstrained HERE because cc_buyback pins
    /// it against CC's on-chain allow-list, and the same value is bound into the
    /// signed quote — two independent checks, neither of them this program's.
    pub destination_owner: UncheckedAccount<'info>,
    #[account(address = config.usdc_mint)]
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    /// CC's treasury token account. Its authority is cc_buyback's policy PDA,
    /// acting as an spl-token delegate; the account's OWNER stays CC's wallet,
    /// which is what keeps CC's webhook reconciliation matching.
    #[account(mut)]
    pub cc_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    /// The vault's canonical USDC ATA — the only place the payment can land.
    #[account(
        mut,
        associated_token::mint = usdc_mint,
        associated_token::authority = vault,
        associated_token::token_program = token_program,
    )]
    pub vault_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: Core AssetV1 — ownership validated by the mpl-core CPI.
    #[account(mut, owner = MPL_CORE_ID)]
    pub asset: UncheckedAccount<'info>,
    /// CHECK: the asset's Core collection; mpl-core binds it to the asset.
    pub collection: UncheckedAccount<'info>,
    /// CHECK: pinned to the mpl-core program id.
    #[account(address = MPL_CORE_ID)]
    pub mpl_core_program: UncheckedAccount<'info>,
    /// CHECK: pinned in the handler to CC_BUYBACK_ID.
    pub cc_program: UncheckedAccount<'info>,
    /// CHECK: cc_buyback's policy PDA. The owner check is here; cc_buyback's own
    /// `seeds = [b"policy"]` constraint pins which account it must be, so a
    /// different cc_buyback-owned account is rejected there rather than passing
    /// silently.
    #[account(owner = CC_BUYBACK_ID @ VaultError::CcPolicyMismatch)]
    pub cc_policy: UncheckedAccount<'info>,
    /// CHECK: cc_buyback's `[b"quote", digest]` marker. Not derivable here — the
    /// digest is recomputed inside cc_buyback — so cc_buyback verifies the
    /// address. Empty on the way in; cc_buyback allocates it.
    #[account(mut)]
    pub cc_quote_marker: UncheckedAccount<'info>,
    /// CHECK: cc_buyback's `[b"rent"]` vault, which fronts the marker's rent so
    /// the phone does not pay for CC's bookkeeping.
    #[account(mut, seeds = [CC_RENT_VAULT_SEED], bump, seeds::program = CC_BUYBACK_ID)]
    pub cc_rent_vault: UncheckedAccount<'info>,
    /// CHECK: pinned to the instructions sysvar. cc_buyback reads the ed25519
    /// quote instruction through it.
    #[account(address = sysvar::instructions::ID)]
    pub sysvar_instructions: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct WithdrawToken<'info> {
    #[account(
        seeds = [VAULT_SEED, cold_owner.key().as_ref()],
        bump = vault.bump,
        has_one = cold_owner
    )]
    pub vault: Account<'info, Vault>,
    pub cold_owner: Signer<'info>,
    #[account(mut, token::authority = vault, token::mint = mint)]
    pub source: InterfaceAccount<'info, TokenAccount>,
    pub mint: InterfaceAccount<'info, Mint>,
    /// Any token account of the same mint — destination is the cold owner's
    /// free choice, except the source itself (rejected in the handler).
    #[account(mut, token::mint = mint)]
    pub destination: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct WithdrawPnft<'info> {
    #[account(
        seeds = [VAULT_SEED, cold_owner.key().as_ref()],
        bump = vault.bump,
        has_one = cold_owner
    )]
    pub vault: Account<'info, Vault>,
    pub cold_owner: Signer<'info>,
    /// Fee/rent payer (typically the phone) so the cold-signed message stays
    /// as small as possible.
    #[account(mut)]
    pub payer: Signer<'info>,
    pub nft_mint: InterfaceAccount<'info, Mint>,
    #[account(mut, token::authority = vault, token::mint = nft_mint)]
    pub nft_token: InterfaceAccount<'info, TokenAccount>,
    /// CHECK: destination wallet — the cold owner's free choice.
    pub destination_owner: UncheckedAccount<'info>,
    /// CHECK: destination token account (ATA of destination_owner) — created
    /// by the Token Metadata CPI when missing, validated there.
    #[account(mut)]
    pub destination_token: UncheckedAccount<'info>,
    /// CHECK: Metadata PDA — validated by the Token Metadata CPI.
    #[account(mut)]
    pub metadata: UncheckedAccount<'info>,
    /// CHECK: Master edition PDA — validated by the Token Metadata CPI.
    pub edition: UncheckedAccount<'info>,
    /// CHECK: source TokenRecord PDA — validated by the CPI.
    #[account(mut)]
    pub token_record: UncheckedAccount<'info>,
    /// CHECK: destination TokenRecord PDA — validated by the CPI.
    #[account(mut)]
    pub destination_token_record: UncheckedAccount<'info>,
    /// CHECK: optional auth-rules account — validated by the CPI.
    pub authorization_rules: Option<UncheckedAccount<'info>>,
    /// CHECK: when present, pinned to AUTH_RULES_ID by the handler, for
    /// uniformity with BuybackPnft — the one context whose generated
    /// `try_accounts` frame cannot take an `address =` constraint. ABSENT is
    /// encoded by passing THIS PROGRAM's id in this slot (anchor-ts does that
    /// for null); the slot is not last, so it must be filled, never dropped.
    pub authorization_rules_program: Option<UncheckedAccount<'info>>,
    /// CHECK: pinned to the Token Metadata program id.
    #[account(address = mpl_token_metadata::ID)]
    pub token_metadata_program: UncheckedAccount<'info>,
    /// CHECK: pinned to the instructions sysvar id.
    #[account(address = sysvar::instructions::ID)]
    pub sysvar_instructions: UncheckedAccount<'info>,
    /// CHECK: pinned to the associated token program id.
    #[account(address = anchor_spl::associated_token::ID)]
    pub ata_program: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct WithdrawCore<'info> {
    #[account(
        seeds = [VAULT_SEED, cold_owner.key().as_ref()],
        bump = vault.bump,
        has_one = cold_owner
    )]
    pub vault: Account<'info, Vault>,
    pub cold_owner: Signer<'info>,
    /// Fee payer (typically the phone) so the cold-signed message stays small.
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: Core AssetV1 account — owner pinned here, contents and ownership
    /// validated by the mpl-core CPI (it refuses to transfer an asset the vault
    /// does not own).
    #[account(mut, owner = MPL_CORE_ID)]
    pub asset: UncheckedAccount<'info>,
    /// CHECK: the asset's Core collection (CC assets carry a Collection update
    /// authority) — mpl-core binds it to the asset itself; see BuybackCore.
    pub collection: UncheckedAccount<'info>,
    /// CHECK: destination wallet — the cold owner's free choice.
    pub destination_owner: UncheckedAccount<'info>,
    /// CHECK: pinned to the mpl-core program id.
    #[account(address = MPL_CORE_ID)]
    pub mpl_core_program: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct CloseTokenAccountCtx<'info> {
    #[account(
        seeds = [VAULT_SEED, cold_owner.key().as_ref()],
        bump = vault.bump,
        has_one = cold_owner
    )]
    pub vault: Account<'info, Vault>,
    /// Receives the reclaimed rent.
    #[account(mut)]
    pub cold_owner: Signer<'info>,
    #[account(mut, token::authority = vault)]
    pub token: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct SweepPrizeAta<'info> {
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    /// Writable: the closed account's lamports land here first, and only the
    /// rent-exempt minimum is forwarded on to `rent_destination`.
    #[account(mut, seeds = [VAULT_SEED, vault.cold_owner.as_ref()], bump = vault.bump)]
    pub vault: Account<'info, Vault>,
    #[account(mut, token::authority = vault)]
    pub token: InterfaceAccount<'info, TokenAccount>,
    /// CHECK: rent destination, pinned to the CC gacha wallet from the config.
    #[account(mut, address = config.rent_destination)]
    pub rent_destination: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct WithdrawSol<'info> {
    #[account(
        mut,
        seeds = [VAULT_SEED, cold_owner.key().as_ref()],
        bump = vault.bump,
        has_one = cold_owner
    )]
    pub vault: Account<'info, Vault>,
    pub cold_owner: Signer<'info>,
    /// CHECK: recipient is the cold owner's free choice; only receives lamports.
    #[account(mut)]
    pub recipient: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct CloseVault<'info> {
    #[account(
        mut,
        close = cold_owner,
        seeds = [VAULT_SEED, cold_owner.key().as_ref()],
        bump = vault.bump,
        has_one = cold_owner
    )]
    pub vault: Account<'info, Vault>,
    #[account(mut)]
    pub cold_owner: Signer<'info>,
}

// ---------------------------------------------------------------------------
// Events & errors
// ---------------------------------------------------------------------------

#[event]
pub struct VaultInitialized {
    pub vault: Pubkey,
    pub cold_owner: Pubkey,
    pub hot_delegate: Pubkey,
}

#[event]
pub struct HotDelegateRotated {
    pub vault: Pubkey,
    pub hot_delegate: Pubkey,
}

#[event]
pub struct ConfigUpdated {
    pub admin: Pubkey,
    pub pending_admin: Pubkey,
    pub rent_destination: Pubkey,
    pub gacha_usdc_account: Pubkey,
    pub fee_usdc_account: Pubkey,
    pub fee_bps: u16,
    pub paused: bool,
    pub allow_buyback_delegation: bool,
}

#[event]
pub struct AdminTransferred {
    pub previous: Pubkey,
    pub current: Pubkey,
}

#[event]
pub struct PackOpened {
    pub vault: Pubkey,
    pub cold_owner: Pubkey,
    pub amount: u64,
    pub fee: u64,
    pub memo: String,
}

#[event]
pub struct BuybackApproved {
    pub vault: Pubkey,
    pub nft_mint: Pubkey,
}

#[event]
pub struct BuybackExecuted {
    pub vault: Pubkey,
    pub mint: Pubkey,
    pub price: u64,
    pub memo: String,
}

#[event]
pub struct DelegateRevoked {
    pub vault: Pubkey,
    pub token_account: Pubkey,
}

#[event]
pub struct TokenWithdrawn {
    pub vault: Pubkey,
    pub mint: Pubkey,
    pub amount: u64,
}

#[event]
pub struct TokenAccountClosed {
    pub vault: Pubkey,
    pub token_account: Pubkey,
}

#[event]
pub struct PrizeAtaSwept {
    pub vault: Pubkey,
    pub token_account: Pubkey,
    /// Lamports forwarded to `config.rent_destination` — exactly the closed
    /// account's rent-exempt minimum. Anything the account held above it stays
    /// in the vault.
    pub rent_refund: u64,
}

#[event]
pub struct SolWithdrawn {
    pub vault: Pubkey,
    pub recipient: Pubkey,
    pub lamports: u64,
}

#[event]
pub struct VaultClosed {
    pub vault: Pubkey,
    pub cold_owner: Pubkey,
}

#[event]
pub struct BuybackSlotCleared {
    pub vault: Pubkey,
    pub token_account: Pubkey,
}

#[error_code]
pub enum VaultError {
    #[msg("Program is paused")]
    Paused,
    #[msg("Amount must be greater than zero")]
    ZeroAmount,
    #[msg("Memo is empty or too long")]
    InvalidMemo,
    #[msg("Fee basis points out of range")]
    InvalidFeeBps,
    #[msg("Caps must satisfy 0 < per_spin_cap <= daily_cap")]
    InvalidCaps,
    #[msg("Amount exceeds per-spin cap")]
    ExceedsPerSpinCap,
    #[msg("Amount exceeds the daily cap")]
    ExceedsDailyCap,
    #[msg("Arithmetic overflow")]
    MathOverflow,
    #[msg("Token account does not look like a prize NFT")]
    NotAnNft,
    #[msg("Buyback delegation is disabled")]
    BuybackDelegationDisabled,
    #[msg("Another buyback delegate is already live for this vault")]
    BuybackSlotOccupied,
    #[msg("No live buyback delegate to clear")]
    NoLiveBuyback,
    #[msg("Token account is not the one recorded for the live buyback")]
    WrongBuybackToken,
    #[msg("The delegated NFT is still in the vault with a live delegate")]
    BuybackStillActive,
    #[msg("Vault has a live buyback delegate; revoke before closing")]
    VaultNotClean,
    #[msg("Token account is not empty")]
    TokenAccountNotEmpty,
    #[msg("Token account still has a delegate — revoke first")]
    DelegateStillSet,
    #[msg("Insufficient SOL above rent-exempt minimum")]
    InsufficientSol,
    /// Raised at five distinct sites: a signer that is neither cold owner nor
    /// hot delegate (revoke_buyback, clear_buyback_slot), a withdraw_sol
    /// recipient aliasing the vault itself, a withdraw_token destination
    /// aliasing the source, an authorization_rules_program that is not the
    /// pinned mpl-token-auth-rules id, and an initialize_config admin that is
    /// not the program's upgrade authority. Kept as one variant so error code
    /// 6018 stays stable for deployed clients; the message is deliberately
    /// role-neutral.
    #[msg("Account is not authorized for this operation")]
    Unauthorized,
    #[msg("The vault USDC account cannot be swept")]
    CannotSweepUsdc,
    /// No longer reachable: `sweep_prize_ata` forwards exactly the rent and
    /// leaves any excess in the vault instead of refusing. Kept because the
    /// enum is append-only and carries no explicit discriminants — removing or
    /// reordering a variant renumbers every LATER one, so dropping this would
    /// silently move `UnsupportedMint` from 6021 to 6020.
    #[msg("Token account holds lamports above the rent-exempt minimum")]
    ExcessLamports,
    #[msg("USDC mint must be a legacy SPL Token mint")]
    UnsupportedMint,
    #[msg("Quote nonce does not match the vault's current buyback nonce")]
    StaleQuoteNonce,
    #[msg("cc_program is not the Collector Crypt buyback program")]
    CcProgramMismatch,
    #[msg("cc_policy is not owned by the Collector Crypt buyback program")]
    CcPolicyMismatch,
}
