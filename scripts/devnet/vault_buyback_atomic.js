// Live devnet run of the ATOMIC in-program buyback (buyback_core) — the
// endgame path CC's backend will build against ("option 2"). This script
// plays BOTH sides on purpose:
//   - Collector Crypt's side: builds the transaction per the builder contract
//     (idempotent vault-USDC ATA creation paid by cc_authority, a TOP-LEVEL
//     SPL Memo, then the single buyback_core instruction) and signs as
//     cc_authority. The memo convention `<slug>-<uuid>:buyback` mirrors the
//     spin's `:open` leg; the final memo format is CC's to fix.
//   - the user's side: co-signs with the hot key (price consent).
// On devnet one throwaway key is player = cold = hot = admin, and
// config.gacha_wallet is TEMPORARILY swapped to it (we cannot sign as CC's
// wallet), then restored in a finally block. Everything CC's memo +
// pre/post-balance matcher needs is printed at the end.
//
// Run: node scripts/devnet/vault_buyback_atomic.js   (no CC_API_KEY needed)
// Env: ASSET (Core AssetV1 held by the vault), COLLECTION, PRICE (raw 6-dec
// USDC, default 10 USDC), RESTORE_GACHA (wallet to restore into the config).
//
// The throwaway prize wallet's secret key is persisted to
// keys/last-prize-wallet.json BEFORE the buyback, so a crash between the
// buyback and the prize-return step can never strand the asset on a key
// that died with the process.
const fs = require("fs");
const path = require("path");
const {
  Keypair, PublicKey, Transaction, TransactionInstruction, SystemProgram,
} = require("@solana/web3.js");
const { ASSOCIATED_TOKEN_PROGRAM_ID } = require("@solana/spl-token");
const anchor = require("@coral-xyz/anchor");
const { BN } = anchor;
const crypto = require("crypto");
const {
  REPO, USDC, GACHA_WALLET, MEMO_PROG, TOKEN_PROG, MPL_CORE_PROG, load, sleep,
} = require("./common");

const ASSET = new PublicKey(process.env.ASSET || "13fCVtpxtzN8mv8jERe6Ev7rSuXM4nbSwFGhmauvKB7b");
const COLLECTION = new PublicKey(process.env.COLLECTION || "CCryptUfeFSZ3Fgc9FLeKrhLVAP67FSqi1GuVoj9CRac");
const PRICE = new BN(process.env.PRICE || 10_000_000);
const RESTORE_GACHA = new PublicKey(process.env.RESTORE_GACHA || GACHA_WALLET.toBase58());
const PRIZE_WALLET_FILE = path.join(REPO, "keys", "last-prize-wallet.json");

const coreOwner = (info) => new PublicKey(info.data.subarray(1, 33));

// mpl-core TransferV1, same wire format the program itself uses: data [14, 0],
// absent optional accounts passed as the mpl-core program id.
function coreTransferIx(payer, authority, newOwner) {
  return new TransactionInstruction({
    programId: MPL_CORE_PROG,
    keys: [
      { pubkey: ASSET, isSigner: false, isWritable: true },
      { pubkey: COLLECTION, isSigner: false, isWritable: false },
      { pubkey: payer, isSigner: true, isWritable: true },
      { pubkey: authority, isSigner: true, isWritable: false },
      { pubkey: newOwner, isSigner: false, isWritable: false },
      { pubkey: MPL_CORE_PROG, isSigner: false, isWritable: false },
      { pubkey: MPL_CORE_PROG, isSigner: false, isWritable: false },
    ],
    data: Buffer.from([14, 0]),
  });
}

(async () => {
  const { player, conn, program, configPda, vaultPda, vaultUsdc, playerUsdc } = load();
  const send = async (tx, signers, label) => {
    tx.feePayer = player.publicKey;
    tx.recentBlockhash = (await conn.getLatestBlockhash()).blockhash;
    tx.sign(...signers);
    const sig = await conn.sendRawTransaction(tx.serialize());
    const conf = await conn.confirmTransaction(sig, "confirmed");
    if (conf.value.err) throw new Error(`[${label}] ${sig} failed on-chain: ${JSON.stringify(conf.value.err)}`);
    console.log(`[${label}] ${sig}`);
    return sig;
  };

  // [0] Preflight: the vault must own the asset; the "CC" payer needs USDC.
  const assetInfo = await conn.getAccountInfo(ASSET);
  if (!assetInfo || !assetInfo.owner.equals(MPL_CORE_PROG)) throw new Error("ASSET is not an mpl-core account");
  if (!coreOwner(assetInfo).equals(vaultPda)) throw new Error(`asset owner is ${coreOwner(assetInfo)} — not the vault`);
  const ccUsdcBal = BigInt((await conn.getTokenAccountBalance(playerUsdc)).value.amount);
  if (ccUsdcBal < BigInt(PRICE.toString())) throw new Error(`cc payer has ${ccUsdcBal} USDC raw < price`);
  const cfgBefore = await program.account.config.fetch(configPda);
  console.log("[0] config.gacha_wallet before:", cfgBefore.gachaWallet.toBase58());

  let exampleSig = null;
  try {
    // [1] Swap config.gacha_wallet to the throwaway key (admin signature).
    // Inside try: if the swap's rpc() times out but the transaction lands
    // anyway, the finally block still restores the real wallet.
    await program.methods
      .updateConfig(null, player.publicKey, null, null, null, null, null)
      .accountsPartial({ config: configPda, admin: player.publicKey })
      .signers([player])
      .rpc();
    console.log("[1] gacha_wallet -> throwaway (temporary)");

    // [2] A fresh keypair stands in for CC's rotating prize wallet, to show
    // destination_owner is a free per-transaction choice distinct from the
    // signing operator wallet. Its secret key is saved to disk FIRST.
    const prizeWallet = Keypair.generate();
    fs.writeFileSync(PRIZE_WALLET_FILE, JSON.stringify(Array.from(prizeWallet.secretKey)));
    const memo = `dev-${crypto.randomUUID()}:buyback`;
    console.log("[2] destination_owner (rotating prize wallet):", prizeWallet.publicKey.toBase58());
    console.log("[2] its key persisted to", PRIZE_WALLET_FILE);
    console.log("[2] memo:", memo);

    // [3] The transaction, exactly per the builder contract in the IDL docs.
    const createVaultUsdc = new TransactionInstruction({
      programId: ASSOCIATED_TOKEN_PROGRAM_ID,
      keys: [
        { pubkey: player.publicKey, isSigner: true, isWritable: true }, // payer = cc_authority
        { pubkey: vaultUsdc, isSigner: false, isWritable: true },
        { pubkey: vaultPda, isSigner: false, isWritable: false },
        { pubkey: USDC, isSigner: false, isWritable: false },
        { pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
        { pubkey: TOKEN_PROG, isSigner: false, isWritable: false },
      ],
      data: Buffer.from([1]), // CreateIdempotent
    });
    const memoIx = new TransactionInstruction({ programId: MEMO_PROG, keys: [], data: Buffer.from(memo, "utf8") });
    const buybackIx = await program.methods
      .buybackCore(PRICE, memo)
      .accountsPartial({
        config: configPda,
        vault: vaultPda,
        hotDelegate: player.publicKey,
        ccAuthority: player.publicKey,
        destinationOwner: prizeWallet.publicKey,
        usdcMint: USDC,
        ccUsdc: playerUsdc,
        vaultUsdc,
        asset: ASSET,
        collection: COLLECTION,
        mplCoreProgram: MPL_CORE_PROG,
        tokenProgram: TOKEN_PROG,
      })
      .instruction();
    const tx = new Transaction().add(createVaultUsdc, memoIx, buybackIx);
    exampleSig = await send(tx, [player], "3:buyback_core"); // player = cc_authority = hot on devnet

    // [4] Confirm the asset really moved, then IMMEDIATELY send it back into
    // the vault so devnet state stays reusable (plain owner transfer; the
    // prize wallet signs, the player pays the fee). Diagnostics come after.
    const ownerAfter = coreOwner(await conn.getAccountInfo(ASSET));
    if (!ownerAfter.equals(prizeWallet.publicKey)) {
      throw new Error(`asset owner after buyback is ${ownerAfter} — expected the prize wallet; its key is in ${PRIZE_WALLET_FILE}`);
    }
    console.log("[4] asset owner after buyback: prize wallet ✓");
    const back = new Transaction().add(coreTransferIx(player.publicKey, prizeWallet.publicKey, vaultPda));
    await send(back, [player, prizeWallet], "5:prize back to vault");

    // [6] Decode what CC's matcher would see. Purely informational — a
    // failure here must not fail the run (the buyback and the prize return
    // are already confirmed), so it only warns.
    try {
      let parsed = null;
      for (let i = 0; i < 10 && !parsed; i++) {
        parsed = await conn.getParsedTransaction(exampleSig, { commitment: "confirmed", maxSupportedTransactionVersion: 0 });
        if (!parsed) await sleep(2000);
      }
      if (!parsed) throw new Error("getParsedTransaction kept returning null");
      if (parsed.transaction.signatures[0] !== exampleSig) throw new Error("RPC returned a different transaction");
      const pre = parsed.meta.preTokenBalances.find((b) => b.mint === USDC.toBase58() && b.owner === vaultPda.toBase58());
      const post = parsed.meta.postTokenBalances.find((b) => b.mint === USDC.toBase58() && b.owner === vaultPda.toBase58());
      console.log("[6] vault_usdc delta (pre/post token balances):", (pre?.uiTokenAmount.uiAmount ?? 0), "->", (post?.uiTokenAmount.uiAmount ?? "?"));
      console.log("[6] inner instruction programs:",
        parsed.meta.innerInstructions.map((g) => g.instructions.map((i) => i.programId.toBase58())).flat().join(", "));
      console.log("[6] BuybackExecuted (Program data):",
        (parsed.meta.logMessages.find((l) => l.startsWith("Program data: ")) || "<not in logs>").slice(0, 100));
    } catch (e) {
      console.warn("[6] diagnostics skipped:", e.message);
    }
    return exampleSig;
  } finally {
    // [7] ALWAYS restore the real CC gacha wallet in the config. A failure
    // here is reported with a manual recovery hint and must not mask the
    // original error (or eat a successful run's signature).
    try {
      await program.methods
        .updateConfig(null, RESTORE_GACHA, null, null, null, null, null)
        .accountsPartial({ config: configPda, admin: player.publicKey })
        .signers([player])
        .rpc();
      const cfgAfter = await program.account.config.fetch(configPda);
      console.log("[7] config.gacha_wallet restored:", cfgAfter.gachaWallet.toBase58());
    } catch (e) {
      console.error("[7] RESTORE FAILED — config.gacha_wallet is still the throwaway key!");
      console.error(`    Fix by hand: RESTORE_GACHA=${RESTORE_GACHA.toBase58()} and re-run, or call update_config directly.`);
      console.error("    Restore error:", e.message);
      if (exampleSig) console.log("\nDONE (restore pending). Example signature for CC:", exampleSig);
    }
  }
})().then(
  (sig) => console.log("\nDONE. Example signature for CC:", sig),
  (e) => { console.error(e); process.exit(1); },
);
