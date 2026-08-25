# Tangem Gacha Vault

Anchor program for the **Tangem Collectibles × Collector Crypt** integration.
It gives every Tangem user a per-user smart-contract vault (PDA) so that gacha
spins and buybacks run off a **hot delegate key** on the phone, while the
**cold key** (Tangem card) stays the root owner of all assets — no card tap per
spin.

## Integration status

Everything on our side is implemented, tested, and live on devnet
(program `29agFEruMu2jedVwnDgKPuq7ejmTiB7sEqzdhufQQS9u`; current binary
deployed 24-08-2026 at slot 487301012, 671312 bytes, sha256
`6e8b6aa14505a66fcc1d1c6e53c3f736aafdc7896cfc8128015ee7db2a96f737`, bytes
verified against a dump). The IDL is committed at
`idl/tangem_gacha_vault.json` and kept in sync on redeploys.

- **Spin** — verified end-to-end against CC's dev backend (17-07-2026):
  `open_pack` pays from the vault via inner CPI, CC credits the spin and
  delivers the prize to the vault PDA.
- **Atomic buyback (`buyback_pnft` / `buyback_core`)** — the endgame path.
  `buyback_core` has reference live runs on devnet against the real Core
  prize in its current form (`destination_owner` included):
  `5AvPAqJjWkaDeKpSr5F7bs7NyrS8Rhi9KqEYQ6pZBHcqAtKjVHoizo8p1rXigQFQDxDBa7JG9UuFPLfbL2ZK3sg1`
  (24-08-2026) and
  `2xvN93qri67MWsUQXMR1Cw4ohdkhPntbA8HTypeRYwU2NoSmU9uad6foFdKUrrFuzFVKVupNv12DLRfX7eFa6qLc`
  (22-08-2026); in these runs `cc_authority` was temporarily substituted,
  since we cannot sign with CC's wallet. `buyback_pnft` has not been executed
  live — its first run is the joint e2e with CC (the litesvm happy paths
  cover the full current account list).
- **pNFT buyback via CC's template** — the legacy path, verified end-to-end
  (28-07-2026): `approve_buyback` → `/api/buyback` with `transferAuthority`
  (no-closeAccount template, scoped API key) → hot countersign → prize to CC,
  refund in the vault, slot cleared, ATA closed. Remains only until CC's
  backend migrates to the atomic path.
- **`sweep_prize_ata`** — our half of CC's rent-recovery cron; permissionless,
  so CC's cron calls it directly. Live, verified by a real sweep on devnet.

CC confirmed (19-08-2026): their backend will build buyback transactions
against these instructions via a new endpoint, driven by our committed IDL;
the wallet that signs buybacks is the same gacha wallet the USDC goes to, and
their rent cron keys on it too — matching the single `config.gacha_wallet`
pin the program enforces. The remaining dependency is that endpoint, plus
pinning `config.gacha_wallet` to the wallet CC actually signs with. Read
"Security model" before deploying — `gacha_wallet` is an asset-custody
authority, not a payment address.

Reference scripts for both halves of the protocol live in `scripts/devnet/`:
`vault_buyback_atomic.js` (builds the atomic buyback transaction — the
CC-side role) and `vault_cosign_buyback.js` (validates the built transaction
against a whitelist and co-signs with the hot key — the Tangem-side role);
`vault_cosign_demo.js` rehearses the full two-party protocol on devnet with
the two roles held by separate keys.

## Verified integration facts (July 2026)

Everything below was verified against Collector Crypt's live API, docs
(`docs.collectorcrypt.com/gacha/api`), frontend bundles, and decoded mainnet
transactions — not assumptions:

1. **CC's gacha has no on-chain program.** All gacha flows are backend-built,
   backend-co-signed ordinary transactions (Memo + SPL Token + Token Metadata
   only).
2. **Spin (`:open`)**: `POST /api/generatePack {playerAddress, packType,
   turbo?, altPlayerAddress?, altFundsRecipient?}` returns
   `{memo: "<slug>-<uuid>", transaction}` — a tx partially signed by
   `GachaNgyXTU3zFogQ8Z5jR2BLXs8215X2AtEH18VxJq3`, whose memo instruction
   *requires* that key's signature; the USDC transfer is a plain **top-level**
   transfer from the player's ATA to treasury ATA
   `D9CEogjHA6CpS12F8St9zpSco7pQKJB5uR1RqDsuzQZk`. The player countersigns as
   fee payer and transfer authority, then `POST /api/submitTransaction`, then
   `POST /api/openPack {memo}`.
3. **The API's `memo` field is the bare `<slug>-<uuid>` join key.** The
   on-chain memo per leg appends a suffix: `:open` / `:send:r<hex>` /
   `:buyback`. Whoever builds the payment tx must append `":open"` itself.
   That join key drives all reconciliation (`/api/pack/status?memo=`,
   `/api/stats`, `/api/vrf/verify?memo=`).
4. **Prizes are MOSTLY Metaplex Core assets now** (per CC, 17-07-2026;
   AssetV1, collection `CCryptUfeFSZ3Fgc9FLeKrhLVAP67FSqi1GuVoj9CRac`, no
   token accounts, ships unfrozen — but the collection carries
   `PermanentTransferDelegate`/`PermanentBurnDelegate` with authority = CC,
   so CC retains seize/burn power over Core prizes regardless of custody).
   Older prizes are **Token Metadata pNFTs**, verified collection
   `CCryptWBYktukHDQ2vHGtVcmtjXxYzvw8XNVY64YN2Yf`, ruleset = Metaplex
   Foundation Rule Set (`eBJLFYPx…`, currently blocks nothing); their token
   accounts are **permanently frozen** — every move goes through Token
   Metadata `TransferV1`; SPL `approve`/`transfer` do not work; delegation =
   `DelegateV1` with `TokenDelegateRole::Transfer` written into the per-token
   `TokenRecord` PDA (auto-cleared on transfer). The machines can also emit
   compressed NFTs but do not today (candidate for future $5 packs).
5. **A PDA can receive prizes** (recipient never signs the `:send` leg;
   `altPlayerAddress` is an explicit API field; ME/Tensor escrow PDAs hold CC
   pNFTs today). `/api/buyback` takes an optional **`transferAuthority`** (a
   delegate that countersigns the NFT transfer instead of the owner) and
   builds for off-curve owners, and CC scopes a no-closeAccount template to
   our API key — which is what makes the legacy template path work at all.
6. Buyback eligibility (per CC, 21-07-2026): **any current holder**, within
   ~3 days of the prize leaving the gacha (the receiver-based rule in the docs
   is stale); the `/api/buyback/available` wallet param is to become optional.
   Refund = `instantBuyback%` (85/90/93 by pack tier) of insured value, capped
   at 40 000 USDC; `altRecipient` redirects the USDC refund only and is an
   **owner wallet** — an off-curve PDA works (CC derives and creates the ATA;
   verified by decoding the built tx).
7. Randomness: their own `cc-vrf` (RFC 9381 ECVRF), commit tx per spin by their
   operator key; verification is public per memo — nothing VRF-related touches
   the payer transaction.

## Architecture

```
cold key (Tangem card)  ──owns──►  Vault PDA ["vault", cold_owner]
                                     ├── USDC ATA   (spins are paid from here)
                                     └── pNFT ATAs  (prizes land here)
hot key (phone)         ──delegate── limited actions only
```

| Action | Signer | Instruction | Funds can go to |
|---|---|---|---|
| Create vault | cold (+phone pays rent) | `init_vault` | — |
| Deposit USDC | cold (plain SPL transfer, 1 tap) | none needed | vault ATA |
| Spin | hot | `open_pack` | CC treasury + fee treasury only |
| Buyback (atomic, endgame) | CC operator + hot (consent) | `buyback_pnft` / `buyback_core` | NFT → `destination_owner` (CC's free per-transaction choice — their prizes live across rotating prize wallets), USDC → the vault's canonical ATA (pinned). The signer is pinned to `config.gacha_wallet` — but that field is admin-writable, so the pin is only as strong as the admin key |
| Prepare buyback (legacy template path) | hot | `approve_buyback` (pNFT `DelegateV1`) | — |
| Free buyback slot | hot or cold | `revoke_buyback` / `clear_buyback_slot` | — |
| Withdraw USDC | cold | `withdraw_token` | anywhere |
| Withdraw prize pNFT | cold | `withdraw_pnft` (`TransferV1`) | anywhere |
| Withdraw prize Core asset | cold | `withdraw_core` (mpl-core `TransferV1`) | anywhere (not the only Core exit — see `buyback_core`) |
| GC empty token accounts | cold | `close_token_account` | everything → cold owner |
| Sweep empty prize ATAs | anyone (permissionless; CC's cron) | `sweep_prize_ata` | exactly the rent → CC gacha wallet; any excess stays in the vault |
| Sweep excess SOL | cold | `withdraw_sol` | anywhere (rent floor stays) |
| Close vault (offboarding) | cold | `close_vault` | rent → cold owner |
| Rotate hot key / caps | cold | `update_vault` | — |
| Pause / feature flags | Tangem admin | `update_config` | — |

Caps: `per_spin_cap` and `daily_cap` (UTC-day bucket) bound everything the hot
key can spend — `open_pack` charges `amount + fee` in one check, and a new UTC
day starts with a fresh budget. As with any calendar-bucket limiter, a sliding
24h window can still see up to ~2× `daily_cap` across a day boundary — size
the cap with that in mind.

## The spin payment (verified end-to-end on CC's dev backend, 17-07-2026)

Our backend calls `generatePack` (with `altPlayerAddress` = **vault PDA** so
the prize lands under cold-key control) only to obtain the memo, DISCARDS CC's
transaction, and the phone sends our own: a client-added TOP-LEVEL memo
(`<memo>:open` — we append the suffix) + `open_pack`, which transfers USDC to
the **whitelisted** treasury ATA via inner CPI and takes the Tangem fee
atomically. CC's webhook credits the spin by matching memo + treasury
transfer; their indexer parses inner (CPI) transfers, needs no GachaNgy
co-signature and no trusted-payor registration. Submit via any RPC — CC's
`/api/submitTransaction` rejects foreign signatures. Guarantees: destination
fixed on-chain, fee atomic, caps enforced in the same instruction.

**History.** A fallback "Plan B" (`approve_usdc_spending`: SPL-delegate to the
hot key + a top-level delegated transfer, working against CC's pre-17-07
top-level-only indexer) was implemented, verified on devnet, and then REMOVED
once the CPI path was confirmed: it was strictly weaker (a delegate can send
the allowance anywhere; the whole `outstanding_approval` day-carry machinery
existed only to bound that). `Config._reserved` (bool) and `Vault._reserved`
(u64) are layout padding where its state lived — kept so already-deployed
accounts stay compatible.

## Buyback — atomic path (`buyback_pnft` / `buyback_core`)

This is the endgame path CC's backend is migrating to. Atomic swap in one
instruction: CC pays `price` USDC into the vault's canonical USDC ATA
(pinned), then the vault PDA signs the NFT transfer as the OWNER — no
standing delegate exists at any point. Two signers:

- **`cc_authority`** — the CC operator wallet fixed in the config
  (`config.gacha_wallet`). It pays the refund and the fees; on the pNFT path
  it also receives the closed prize ATA's rent (CC fronted that rent at
  delivery).
- **`hot_delegate`** — user consent to the offered price. The phone MUST
  decode the transaction and display the `price` ARGUMENT before signing
  (nothing on-chain bounds `price` beyond `> 0`).

The NFT lands at **`destination_owner`** (account slot 5 in both
instructions), deliberately unconstrained: CC confirmed (19-08-2026) that
prizes return to rotating prize wallets — "could be 10 different wallets and
it changes often" — not to the signing operator wallet. On the pNFT path the
emptied prize ATA is closed in the same instruction (lamports back to CC) and
a stale `approve_buyback` slot pointing at this token is freed —
`clear_buyback_slot` is NOT needed afterwards.

Builder contract for CC (also encoded in
`scripts/devnet/vault_buyback_atomic.js`):

- Prepend `createAssociatedTokenAccountIdempotent` for the vault's USDC ATA
  (payer = `cc_authority`) — the instruction pins the canonical ATA but does
  not create it.
- `buyback_pnft`: 22 accounts, request ~400k CU (the TransferV1 creates the
  destination ATA + TokenRecord and evaluates the rule set — the 200k default
  does not fit). The full transaction measures **1128 bytes** — inside the
  1232-byte packet, no address lookup tables needed.
- `buyback_core`: 12 accounts, a single light mpl-core CPI — the default
  compute budget suffices.
- Add a top-level memo for CC's own reconciliation. Our devnet runs use
  `<slug>-<uuid>:buyback`, mirroring the spin's `:open`; the final format is
  CC's call. The `memo` argument is additionally recorded in the
  `BuybackExecuted` event.

**Submission protocol** (proposed to CC in August 2026; their written
confirmation pending): CC builds the transaction (their wallet as fee payer, fresh
blockhash) and partially signs → the Tangem endpoint validates it against a
whitelist (`scripts/devnet/vault_cosign_buyback.js` is the reference), the
phone displays the `price` argument and co-signs — a step that can take tens
of seconds → CC submits and retries; if the blockhash expires, CC rebuilds
with a new memo.

## Buyback — legacy template path (to be retired)

The flow below is the delegate-based path that remains only until CC's
backend migrates to the atomic instructions; the moment it does,
`allow_buyback_delegation` should be set `false` permanently, which retires
the standing-delegate drain risk entirely (see "Security model").

1. Prize pNFT sits in the vault's ATA (frozen, owner = vault PDA).
2. Hot key calls `approve_buyback` → vault CPIs `DelegateTransferV1`
   (invoke_signed), hot key becomes the Transfer delegate in the TokenRecord.
   Only ONE delegate may be live per vault (`live_buyback_mint` slot).
3. Our backend calls `POST /api/buyback {playerAddress: <vault PDA>,
   nftAddress, altRecipient: <vault PDA>, transferAuthority: <hot key>}`.
   `altRecipient` is an owner wallet; CC derives (and create-idempotents) the
   vault's USDC ATA, off-curve included — verified by decoding the built tx.
4. CC's builder returns: TransferV1 with **authority = `transferAuthority`**
   (+ token record), **fee payer = the gacha wallet** (pre-signed — we do not
   even pay the fee), the USDC refund straight into the vault's ATA, and a
   `:buyback` memo. Two template defects were fixed along the way: a trailing
   `closeAccount` with the gacha as authority (only the ATA owner can close;
   a frozen pNFT ATA cannot be given a close delegate at all) — solved by the
   no-closeAccount template scoped to our API key plus `sweep_prize_ata`; and
   Core assets, over which `transferAuthority` has no rights at all — mooted
   by the atomic `buyback_core`: Core never goes through the template.
5. Hot key countersigns; NFT returns to the prize wallet, USDC lands in the
   vault. The TokenRecord delegate is consumed by the transfer; the client
   then calls `clear_buyback_slot` on **the exact account that was delegated**
   (proof: it is now empty / delegate-free) to free the vault's slot.
6. The emptied NFT ATA is later closed. **Policy:** CC fronted the rent for
   every prize ATA, so the app should call `sweep_prize_ata` (rent → CC) for
   those and reserve `close_token_account` (everything → cold owner) for
   accounts the user paid for, such as the USDC ATA at offboarding. Nothing
   on-chain arbitrates between the two — whoever calls first wins — so this
   is a client-side commitment. Either way it must happen only *after* the
   slot is freed (closing it first would strand the slot). The TokenRecord
   takes care of itself: TransferV1 closes the source record and refunds its
   rent to the transfer's fee payer — verified in
   `programs/tangem-gacha-vault/tests/pnft.rs`, both for
   the delegate-authority and the owner path.

**Offboarding order:** `revoke_buyback`/`clear_buyback_slot` (free the slot)
→ `withdraw_token` / `withdraw_pnft` / `withdraw_core` → **`sweep_prize_ata`
for CC-created prize ATAs and `close_token_account` for the USDC ATA and
anything the user paid for** (per the policy in step 6 above) →
`close_vault`. `close_vault` refuses to run while a buyback delegate is still
live, so a close + re-init cannot silently reset the slot while an old
hot-key Metaplex delegate stays live on-chain. Rotating the hot key via
`update_vault` changes who may *call* instructions but does NOT revoke an
already-granted pNFT transfer delegate — pair a rotation with
`revoke_buyback` if the old key may be compromised.

## Physical card redemption — known gap (roadmap)

CC's Shipping API (`docs.collectorcrypt.com/vault/shipping-api`) requires the
card to be **owned by an on-curve wallet on the user's CC user row**: the
wallet authenticates (Privy or SIWS message-signing) and signs the
burn+shipping-payment transactions built by `/redeem/prepare`. A program PDA
can satisfy neither requirement, so **redemption of a vault-held prize has no
direct path today**. Interim flow: `withdraw_pnft` to the cold wallet (1 tap)
→ SIWS-authenticate the cold wallet → cold key signs CC's burn+payment txs
(1-2 more taps; tx size vs the card budget unvalidated; blockhash expiry vs
cold-signing latency is an open question to CC). If CC supports a
**Utility-role burn delegate** (open question to CC; for Core prizes their
collection-level `PermanentBurnDelegate` could serve the same role), a future
`redeem_physical` instruction (cold-gated — redemption redirects a physical
asset to a postal address, so it must NOT be hot-key-triggerable) could reduce
this to one tap. Blocked on CC-side answers — see "Open questions to CC".

## Security model — honest worst cases

- **Cold key**: root authority; can withdraw/rotate/close at any time; never
  needed for spins/buybacks.
- **Hot-key compromise**:
  - USDC: can spend the deposited balance on gacha spins within per-spin and
    daily caps, and ONLY to the whitelisted treasury/fee accounts fixed in the
    config. It can NOT withdraw USDC or redirect it anywhere else. **But the
    caps are a theft budget, not a waste budget.** The prize *recipient* is
    `altPlayerAddress`, chosen off-chain when the memo is minted, and CC
    credits whichever memo accompanies a treasury transfer regardless of who
    paid (verified on-chain). A stolen phone can therefore mint its own memos
    and spend the vault's USDC on spins whose prizes are delivered to the
    attacker's own wallet — realizing ~100% of `daily_cap` as value, and CC's
    any-holder buyback turns those prizes straight back into USDC. This cannot
    be closed on-chain (the memo carries no recipient the program can check);
    the control is operational: our backend must mint memos ONLY with
    `altPlayerAddress` = the vault PDA, index every `PackOpened.memo`, and
    alarm on a memo it did not issue or a prize not delivered to the PDA.
    Size `daily_cap` accordingly.
  - Prize pNFTs: **can be drained, serially** (legacy path only).
    `approve_buyback` lets the hot key make itself a transfer delegate and a
    transfer delegate can move that pNFT anywhere (the CC ruleset blocks
    nothing) — approve + transfer even fits in one atomic tx, so
    pause/revoke are reactive. The `live_buyback_mint` slot forces one NFT at
    a time (the next approval needs on-chain proof the previous NFT left),
    and `allow_buyback_delegation` is the admin kill switch for **this path
    only** — these bound the *rate*, not the outcome. Eliminating this
    entirely would require a cold-key tap per buyback; the product accepts
    the risk for NFTs, whose intended lifecycle in the vault is short (72h
    buyback window), and long-term holdings should be moved to cold storage
    via `withdraw_pnft`. This path is retired the moment CC's backend moves
    to `buyback_pnft` — then set `allow_buyback_delegation = false`
    permanently.
  - Prizes via atomic buyback: the hot key alone cannot move one —
    `buyback_pnft` / `buyback_core` also need CC's signature, the refund
    destination is pinned (the vault's canonical USDC ATA) and the payment
    lands there first; the NFT goes to whatever `destination_owner` the CC
    signer chose. See the admin bullet for the case where both keys are held.
- **Tangem admin key — larger than it looks.** It cannot touch assets by
  itself, but `config.gacha_wallet` is **an asset-custody authority, not a
  payment address**: it is the `address =` pin on `cc_authority`, i.e. the
  only key that may execute a buyback — a buyback that sends the prize to an
  unconstrained `destination_owner` of that signer's choosing.
  `update_config` rewrites it with one signature, no timelock, for every
  vault at once (Config is a singleton). So **admin + a vault's hot key can
  move any prize out of that vault at any `price > 0`** — no cold tap, no
  caps (they apply only to `open_pack`), no one-live-delegate slot, no price
  floor. In particular this is the first path by which anything other than
  the cold key can move a **Core** asset: before these instructions
  `withdraw_core` was their sole exit. Both keys are Tangem-controlled
  artefacts, so this is a single-organization compromise — precisely what
  the cold card is meant to prevent. Consequences:
  - **Hold the admin key in a multisig (Squads) — a prerequisite, not a
    nicety** — and alarm on `ConfigUpdated`, treating a change to ANY of
    `gacha_wallet`, `gacha_usdc_account` or `fee_usdc_account` as a
    break-glass event rather than routine config.
  - Incident levers, least to most blunt: repoint `gacha_wallet` (disables
    both buybacks globally and moves the sweep rent away, while spins keep
    working — `open_pack` pins the separate `gacha_usdc_account`); `paused`
    (stops spins and buybacks for everyone); per vault, a cold-key
    `update_vault` hot rotation. `allow_buyback_delegation` is **not** a
    brake here.
  - **The admin key reaches value on a second, independent path.**
    `update_config` also rewrites `gacha_usdc_account` and
    `fee_usdc_account`, and those two pins are the ONLY destination checks
    `open_pack` performs. One admin signature therefore diverts 100% of every
    FUTURE spin payment — principal and fee, every vault, Config being a
    singleton — to an attacker-owned USDC account, with no hot key involved
    and no cap: users keep spinning, CC never credits the spins, and the
    money lands elsewhere. `MAX_FEE_BPS` bounds the fee rate, not this.
    Repointing `gacha_wallet` (above) is the buyback-side equivalent; both
    are one-step and untimelocked.
  - The fee stays hard-capped at `MAX_FEE_BPS` (10%); there is no analogous
    ceiling on the buyback path.
  - Admin handover is one-step and irreversible (`initialize_config` is
    `init` at fixed seeds and the config can never be closed or re-created),
    so a mistyped `new_admin` permanently freezes pause, fees, treasuries and
    `gacha_wallet`. Only ever set it to a multisig that has already proven it
    can sign a no-op `update_config` on devnet.
  - `initialize_config` is gated to the program's upgrade authority, so the
    admin slot cannot be front-run at deploy time.
- **Buyback scope**: `buyback_pnft`/`buyback_core` pin no collection or
  creator, so any NFT the user parks in the vault — not just CC prizes — is
  inside that blast radius. The phone must refuse to co-sign for an asset its
  inventory does not tag as a CC prize.
- **Client contract for CC-built transactions**: the phone's one signature
  covers the whole transaction, and CC builds it. The phone MUST decode it,
  display the `price` **argument** (not the API quote), and refuse any
  transaction carrying an instruction of this program other than the single
  buyback it is showing — consent is instruction-scoped, not
  transaction-scoped, and a bundled `open_pack` would be signed by the same
  hot key and would send vault USDC to CC's treasury.
- **Collector Crypt itself**: for Core prizes CC holds collection-level
  `PermanentTransferDelegate`/`PermanentBurnDelegate`, so it can seize or
  burn them regardless of custody. For Core prizes the vault protects
  against key theft, not against CC.
- All PDAs derive from the cold key: losing the phone loses nothing;
  `close_vault` + re-`init_vault` with the same cold key restores access to
  anything delivered late.

## Cold-key transaction size

Tangem cards have a tighter message-size budget than Solana. Cold-signed
messages here are: a plain USDC transfer (deposit), `init_vault` (4 accounts),
`update_vault`, `withdraw_token` (6 accounts), `close_token_account`
(4 accounts), `withdraw_sol` (3 accounts), `close_vault` (2 accounts),
`withdraw_core` (7 accounts, a single CPI — far lighter than the pNFT path,
no compute-budget instruction needed), and `withdraw_pnft`.

**withdraw_pnft measured** (`programs/tangem-gacha-vault/tests/pnft.rs`,
18 accounts incl. the Metaplex
Foundation Rule Set that CC cards carry, + the REQUIRED compute-budget
instruction — the pNFT TransferV1 does not fit the default 200k CU):
**682-byte cold-signed message, 811-byte serialized transaction** (Solana cap
1232). The card's signing budget was confirmed at 928/964 bytes (28-07-2026),
so this fits with room to spare — no address lookup table and no
versioned-transaction support needed on the card side. Budget ≥300k CU for
the transaction.

The atomic buybacks are hot-signed, not cold-signed, so the card budget does
not apply — but for the record, the full 22-account `buyback_pnft`
transaction measures 1128 bytes: inside the packet, no lookup tables.

## Open questions to CC

Most integration questions were closed empirically on CC's dev backend
(vault-paid spins, off-curve `altPlayerAddress` and `altRecipient`,
`transferAuthority`, the no-closeAccount template) or answered directly
(rotating prize wallets, single gacha wallet for signing and rent cron, the
IDL-driven buyback endpoint). Still open:

1. **Written confirmation of the submission protocol** (proposed to CC in
   August 2026): CC builds (gacha wallet as fee payer, fresh blockhash) and
   partially signs → Tangem's endpoint validates against a whitelist, the
   phone displays the `price` argument and co-signs (tens of seconds) → CC
   submits and retries; on an expired blockhash CC rebuilds with a new memo.
2. **Final buyback memo format.** Our runs use `<slug>-<uuid>:buyback`
   (mirroring the spin's `:open`); the final convention is CC's call.
3. **Royalties rule set — standing dependency, to confirm at the first joint
   e2e.** mpl-core's Royalties plugin evaluates the OWNER PROGRAM of the
   transfer authority; ours is the vault PDA, owned by this program. CC's
   Core collection carries a permissive rule set today; if CC ever switches
   it to a `ProgramAllowList` that omits our program id, BOTH `withdraw_core`
   and `buyback_core` break for every vault — including the cold-key rescue
   path. CC's answer (19-08-2026): "I think our royalties rule will allow
   that" — to be confirmed live (our litesvm suites already pass against the
   real dumped CC collection with its live Royalties plugin), and the
   collection's plugins must not change without telling us.
4. Whether `altFundsRecipient` (turbo proceeds) is an owner wallet like
   `altRecipient`, and off-curve-safe.
5. Physical redemption for vault-held prizes: burn-delegate path or
   eligibility after `withdraw_pnft` (see the roadmap section above).
6. Prod parity of the 17-07 webhook fixes — not asked; will be verified by a
   cheapest-pack probe before the mainnet launch.

## Mainnet TODO

**Layout freeze — decide before the first mainnet Config/Vault exists.**
These are free now and impossible (or a per-user card tap) later, because the
program has no realloc and no migration instruction:

- **Growth padding.** `Config` (174 B) and `Vault` (179 B) are allocated at
  exactly `8 + INIT_SPACE`, with no spare byte. `Vault`'s maximum
  serialization with both `Option<Pubkey>`s set is exactly 179 B, so adding
  any field later makes `Account::exit` fail for every existing vault. Append
  explicit padding (e.g. `[u8; 64]` / `[u8; 32]`) at the freeze.
- **Two-step admin handover** (`pending_admin` + `accept_admin`), since the
  current one-step write is irreversible.
- **`config.usdc_mint` is permanent** — written once in `initialize_config`,
  with no `update_config` parameter, no `close_config`, and `init` (not
  `init_if_needed`) at fixed seeds. A wrong mint is only recoverable by
  redeploying under a new program id, which changes every vault PDA. The
  program rejects a non-legacy-SPL mint at init; still verify the value by
  hand. Same care for `gacha_usdc_account` / `fee_usdc_account`: they are
  bare pubkeys validated only when `open_pack` runs. A malformed one halts
  spins — non-token account → 3007, wrong mint → 2014 on the very first spin.
  A well-formed USDC account owned by the wrong party is worse — spins keep
  succeeding and every payment silently goes to that owner instead of CC, so
  the failure surfaces only as CC not crediting the spins.
- **mpl-core wire format.** The two Core CPIs are hand-encoded (`data =
  [14, 0]` + a 7-meta account list) with no `mpl-core` crate dependency, so a
  Metaplex-side change to `TransferV1` would break both Core paths with no
  compile-time or Cargo.lock signal — and the tests could not catch it
  either, since they run the DUMPED program from `tests/fixtures/`. Covered
  by `./scripts/check_fixtures.sh`, which re-dumps all three upstream
  programs from mainnet-beta and exits non-zero on any hash change: **run it
  before every deploy.** Taking the `mpl-core` dependency and using its
  generated CpiBuilder would make this a compile-time check instead — worth
  doing at the mainnet freeze if the extra dependency is acceptable.

**Operational prerequisites:**

- Squads 2/2 on the upgrade authority (agreed with CC, to set up towards
  mainnet) and a multisig behind the admin key — see "Security model".
- Pin `config.gacha_wallet` to the wallet CC actually signs buybacks with.
- Cheapest-pack probe on prod to confirm webhook parity (open question 6).
- External audit before mainnet.

## Build & test

Requires Rust + Solana CLI (Agave) 2.x + Anchor 0.31.1.

```bash
# The program keypair lives in keys/, target/ is gitignored. Restore it FIRST:
# on a tree without it, `anchor keys sync` mints a NEW program id and rewrites
# declare_id! + Anchor.toml away from 29agFEru… .
mkdir -p target/deploy && cp keys/tangem_gacha_vault-keypair.json target/deploy/
anchor keys list          # must print 29agFEruMu2jedVwnDgKPuq7ejmTiB7sEqzdhufQQS9u
anchor build              # → target/deploy/tangem_gacha_vault.so + IDL
                          # NB: rewrites the .so that must stay byte-identical
                          # to devnet — re-verify the dump before redeploying.
yarn install
anchor test               # TS suite; needs a working solana-test-validator
```

**Compile status (verified):** `cargo check`, `cargo build-sbf`, and
`anchor build` all pass against anchor-lang 0.31.1 / anchor-spl 0.31.1 /
mpl-token-metadata 5.1.1, producing the ~671 KB deployable `.so` and the IDL.
Only Anchor's standard benign warnings remain. Note the SBF 4 KB stack limit:
`BuybackPnft` is the widest context (its accounts are Boxed for that reason)
and its generated `try_accounts` sits close to the limit — adding another
`#[account(address = …)]` constraint there overflows the frame (which is why
the auth-rules program id is pinned in the handlers).

**Execution tests:** in-process LiteSVM Rust tests run the real compiled `.so`
without a validator (run `anchor build` first so the `.so` exists):

- `cargo test --test smoke` — init_vault basics.
- `cargo test --test pnft` — the full pNFT lifecycle against the REAL
  mainnet-dumped Metaplex programs and the REAL CC rule set
  (tests/fixtures/, dump commands in Anchor.toml): approve_buyback →
  delegate TransferV1 (CC's buyback leg) → clear_buyback_slot →
  close_token_account; revoke_buyback; the one-live-delegate slot; the
  `allow_buyback_delegation` kill switch; cold-only withdraw_pnft + the
  cold-signed transaction size measurement.
- `cargo test --test caps` — `open_pack` spending caps, incl. the **UTC-day
  rollover** exercised by warping the LiteSVM `Clock`: a full-cap spin is
  refused within the same UTC day and allowed again after midnight; the
  per-spin cap; the memo bounds (empty / 256 / 257 bytes); and a zero amount.
- `cargo test --test sweep` — the permissionless rent sweep: a random caller
  closes an empty prize ATA and exactly its rent lands on the config-pinned CC
  wallet; non-empty / USDC / live-buyback-slot accounts and a substituted rent
  destination are refused; dust on the ATA does NOT block the sweep (CC still
  gets the rent, the excess stays in the vault, and the emitted
  `PrizeAtaSwept.rent_refund` is decoded and pinned to what CC received); and
  an account with a stuck SPL delegate — which `close_token_account` refuses —
  is sweepable, pinning the deliberate asymmetry between the two GC paths.
- `cargo test --test pnft` (buyback part) / `--test core` (buyback part) —
  the atomic `buyback_pnft`/`buyback_core`: happy paths against the real
  Metaplex programs, payment-failure atomicity (an underfunded CC account
  reverts the whole swap — the NFT stays), non-CC signer / missing hot
  consent / paused / zero price / non-canonical vault USDC (pNFT path) all
  refused; a buyback consumes a stale `approve_buyback` delegate and frees
  the slot.
- `cargo test --test core` — `withdraw_core` against the REAL mainnet mpl-core
  program and the REAL CC Core prize + collection accounts dumped from devnet
  (owner patched to the test vault): CC prizes are now MOSTLY Metaplex Core
  (per CC, 17-07-2026), which have no token accounts and ship unfrozen — a
  single-CPI owner transfer. TRUST NOTE: CC's Core collection carries
  PermanentTransferDelegate/PermanentBurnDelegate (authority = CC), so for
  Core prizes the vault protects against key theft, not against CC itself.

All suites load the REAL compiled `.so`, which `cargo test` never rebuilds.
`tests/common/mod.rs` guards that: it compares the binary's mtime against
`src/lib.rs` and `Cargo.toml`, and panics with ``run `anchor build` first``
rather than silently testing the previous build. The same module carries
`decode_event`, used to assert the shape of emitted events.

The full TypeScript suite is `anchor test` (SPL-level auth/caps logic).

## Devnet quickstart against CC

- Devnet program `29agFEruMu2jedVwnDgKPuq7ejmTiB7sEqzdhufQQS9u` runs the
  current binary (see "Integration status" for the deployed hash); the IDL is
  committed at `idl/tangem_gacha_vault.json` (regenerated into `target/idl`
  by `anchor build` — keep the committed copy in sync on redeploys). The
  deployed `Config` (174 B) and `Vault` (179 B) layouts and the append-only
  error enum are the constraints on any change that must keep those accounts
  valid.
- ⚠️ **Before any redeploy, check the programdata size.** The binary has
  outgrown the account more than once (`solana program extend` costs
  non-refundable rent each time, ~0.31 SOL for +45000). Run
  `solana program show <id> --url devnet`, compare `Data Length` with the
  local `.so`, and extend with slack rather than exactly. Budget ~4.4 SOL
  temporarily for the deploy buffer (refunded).
- Reference scripts: `scripts/devnet/vault_buyback_atomic.js` builds the
  atomic buyback transaction (the CC-side role);
  `scripts/devnet/vault_cosign_buyback.js` validates a built transaction
  against a whitelist and co-signs with the hot key (the Tangem-side role).
- Base URL: `https://dev-gacha.collectorcrypt.com/api`
- Dev USDC mint: `Gh9ZwEmdLJ8DscKNTkTqPbNwLNNBjuSzaG9Vp2KGtKJr` (faucet:
  spl-token-faucet.com)
- Dev gacha wallet: `A4ahkivAG4NoZAE8Sy4qv8nn2DU9yoXRQcttuCeGtTJv`
- The x-api-key issued by CC sets our memo slug (attribution + rev-share
  filter).
- When decoding txs via public RPC, verify `signatures[0]` matches the
  requested signature — mainnet-beta was observed returning a wrong tx once.
