// Two-party rehearsal of the buyback submission protocol on devnet, with the
// two roles held by two DIFFERENT keys (unlike vault_buyback_atomic.js, where
// one throwaway key plays everyone):
//   CC side   = the throwaway player key (config.gacha_wallet is temporarily
//               swapped to it): builds the transaction per the builder
//               contract, partially signs, exports base64;
//   Tangem side = a freshly generated hot key (the vault's hot_delegate is
//               temporarily rotated to it): the co-signer validates the
//               whitelist, co-signs and submits — via the exact module
//               functions of scripts/devnet/vault_cosign_buyback.js.
// Also exercises offline NEGATIVES of the validator (foreign instruction,
// hot as a source of funds) before touching the chain.
//
// Run: node scripts/devnet/vault_cosign_demo.js     (no CC_API_KEY needed)
// Env: ASSET, COLLECTION, PRICE — as in vault_buyback_atomic.js.
const fs = require("fs");
const path = require("path");
const {
  Keypair, PublicKey, Transaction, TransactionInstruction, SystemProgram,
} = require("@solana/web3.js");
const { ASSOCIATED_TOKEN_PROGRAM_ID } = require("@solana/spl-token");
const anchor = require("@coral-xyz/anchor");
const { BN } = anchor;
const crypto = require("crypto");
const { REPO, USDC, GACHA_WALLET, MEMO_PROG, TOKEN_PROG, MPL_CORE_PROG, load } = require("./common");
const { validateBuybackTx, cosign } = require("./vault_cosign_buyback");

const ASSET = new PublicKey(process.env.ASSET || "13fCVtpxtzN8mv8jERe6Ev7rSuXM4nbSwFGhmauvKB7b");
const COLLECTION = new PublicKey(process.env.COLLECTION || "CCryptUfeFSZ3Fgc9FLeKrhLVAP67FSqi1GuVoj9CRac");
const PRICE = new BN(process.env.PRICE || 10_000_000);

const coreOwner = (info) => new PublicKey(info.data.subarray(1, 33));

(async () => {
  const { player, conn, program, configPda, vaultPda, vaultUsdc, playerUsdc } = load();
  const send = async (tx, signers, label) => {
    tx.feePayer = player.publicKey;
    tx.recentBlockhash = (await conn.getLatestBlockhash()).blockhash;
    tx.sign(...signers);
    const sig = await conn.sendRawTransaction(tx.serialize());
    const conf = await conn.confirmTransaction(sig, "confirmed");
    if (conf.value.err) throw new Error(`[${label}] ${sig} failed: ${JSON.stringify(conf.value.err)}`);
    console.log(`[${label}] ${sig}`);
    return sig;
  };

  // Preflight (same as the atomic script).
  const assetInfo = await conn.getAccountInfo(ASSET);
  if (!coreOwner(assetInfo).equals(vaultPda)) throw new Error("asset is not vault-owned");

  // The Tangem-side hot key and the CC-side prize wallet — both persisted
  // BEFORE anything moves, so no failure mode strands an asset or the vault.
  const hot2 = Keypair.generate();
  const prizeWallet = Keypair.generate();
  fs.writeFileSync(path.join(REPO, "keys", "last-hot2.json"), JSON.stringify(Array.from(hot2.secretKey)));
  fs.writeFileSync(path.join(REPO, "keys", "last-prize-wallet.json"), JSON.stringify(Array.from(prizeWallet.secretKey)));
  console.log("[0] hot2 (Tangem):", hot2.publicKey.toBase58());
  console.log("[0] prize wallet (CC):", prizeWallet.publicKey.toBase58());

  try {
    // [1] Cold rotates the vault's hot key to hot2; admin swaps the config.
    await program.methods.updateVault(hot2.publicKey, null, null)
      .accountsPartial({ vault: vaultPda, coldOwner: player.publicKey })
      .signers([player]).rpc();
    await program.methods.updateConfig(null, player.publicKey, null, null, null, null, null)
      .accountsPartial({ config: configPda, admin: player.publicKey })
      .signers([player]).rpc();
    console.log("[1] hot -> hot2; gacha_wallet -> throwaway");

    // [2] CC side builds and PARTIALLY signs.
    const memo = `dev-${crypto.randomUUID()}:buyback`;
    const createVaultUsdc = new TransactionInstruction({
      programId: ASSOCIATED_TOKEN_PROGRAM_ID,
      keys: [
        { pubkey: player.publicKey, isSigner: true, isWritable: true },
        { pubkey: vaultUsdc, isSigner: false, isWritable: true },
        { pubkey: vaultPda, isSigner: false, isWritable: false },
        { pubkey: USDC, isSigner: false, isWritable: false },
        { pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
        { pubkey: TOKEN_PROG, isSigner: false, isWritable: false },
      ],
      data: Buffer.from([1]),
    });
    const memoIx = new TransactionInstruction({ programId: MEMO_PROG, keys: [], data: Buffer.from(memo, "utf8") });
    const buybackIx = await program.methods.buybackCore(PRICE, memo)
      .accountsPartial({
        config: configPda, vault: vaultPda,
        hotDelegate: hot2.publicKey, ccAuthority: player.publicKey,
        destinationOwner: prizeWallet.publicKey,
        usdcMint: USDC, ccUsdc: playerUsdc, vaultUsdc,
        asset: ASSET, collection: COLLECTION,
        mplCoreProgram: MPL_CORE_PROG, tokenProgram: TOKEN_PROG,
      }).instruction();
    const ccTx = new Transaction().add(createVaultUsdc, memoIx, buybackIx);
    ccTx.feePayer = player.publicKey;
    ccTx.recentBlockhash = (await conn.getLatestBlockhash()).blockhash;
    ccTx.partialSign(player); // CC's signature only — hot2 has not signed yet
    const b64 = ccTx.serialize({ requireAllSignatures: false }).toString("base64");
    console.log(`[2] CC built + partially signed (${b64.length} chars base64), memo: ${memo}`);

    // [3] Offline negatives first: the validator must reject tampering.
    const evil1 = Transaction.from(Buffer.from(b64, "base64"));
    evil1.add(SystemProgram.transfer({ fromPubkey: hot2.publicKey, toPubkey: player.publicKey, lamports: 1 }));
    let rejected = 0;
    try { validateBuybackTx(evil1, { hotPubkey: hot2.publicKey, vaultPda, vaultUsdc, programId: program.programId }); }
    catch (e) { rejected += 1; console.log("[3] negative (hot as funds source):", e.message, "✓"); }
    const evil2 = Transaction.from(Buffer.from(b64, "base64"));
    evil2.add(buybackIx); // a second program instruction
    try { validateBuybackTx(evil2, { hotPubkey: hot2.publicKey, vaultPda, vaultUsdc, programId: program.programId }); }
    catch (e) { rejected += 1; console.log("[3] negative (second buyback):", e.message, "✓"); }
    if (rejected !== 2) throw new Error("validator accepted a tampered transaction");

    // [4] Tangem side: validate the REAL transaction, co-sign with hot2, submit.
    const tangemTx = Transaction.from(Buffer.from(b64, "base64"));
    const s = validateBuybackTx(tangemTx, { hotPubkey: hot2.publicKey, vaultPda, vaultUsdc, programId: program.programId });
    console.log(`[4] whitelist OK: buyback_${s.kind}, price ${Number(s.price) / 1e6} USDC -> co-signing as hot2`);
    cosign(tangemTx, hot2);
    const sig = await conn.sendRawTransaction(tangemTx.serialize());
    const conf = await conn.confirmTransaction(sig, "confirmed");
    if (conf.value.err) throw new Error(`buyback failed on-chain: ${JSON.stringify(conf.value.err)}`);
    console.log("[4] submitted:", sig);

    // [5] Verify and send the prize home.
    const ownerAfter = coreOwner(await conn.getAccountInfo(ASSET));
    if (!ownerAfter.equals(prizeWallet.publicKey)) throw new Error(`unexpected owner ${ownerAfter}`);
    const back = new Transaction().add(new TransactionInstruction({
      programId: MPL_CORE_PROG,
      keys: [
        { pubkey: ASSET, isSigner: false, isWritable: true },
        { pubkey: COLLECTION, isSigner: false, isWritable: false },
        { pubkey: player.publicKey, isSigner: true, isWritable: true },
        { pubkey: prizeWallet.publicKey, isSigner: true, isWritable: false },
        { pubkey: vaultPda, isSigner: false, isWritable: false },
        { pubkey: MPL_CORE_PROG, isSigner: false, isWritable: false },
        { pubkey: MPL_CORE_PROG, isSigner: false, isWritable: false },
      ],
      data: Buffer.from([14, 0]),
    }));
    await send(back, [player, prizeWallet], "5:prize back to vault");
    return sig;
  } finally {
    // [6] Restore BOTH: the hot key and the config — each in its own try.
    try {
      await program.methods.updateVault(player.publicKey, null, null)
        .accountsPartial({ vault: vaultPda, coldOwner: player.publicKey })
        .signers([player]).rpc();
      console.log("[6] hot restored to player");
    } catch (e) {
      console.error(`[6] HOT ROTATION RESTORE FAILED — vault hot is still hot2 (key in keys/last-hot2.json): ${e.message}`);
    }
    try {
      await program.methods.updateConfig(null, GACHA_WALLET, null, null, null, null, null)
        .accountsPartial({ config: configPda, admin: player.publicKey })
        .signers([player]).rpc();
      console.log("[6] gacha_wallet restored:", GACHA_WALLET.toBase58());
    } catch (e) {
      console.error(`[6] CONFIG RESTORE FAILED — gacha_wallet is still the throwaway: ${e.message}`);
    }
  }
})().then(
  (sig) => console.log("\nDONE. Two-party co-signed buyback:", sig),
  (e) => { console.error(e); process.exit(1); },
);
