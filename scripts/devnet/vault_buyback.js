// ⚠️ DEVNET DEMO — NOT the client pattern, and NOT the current buyback path.
//
// Two reasons not to copy this into a phone client:
//   1. It runs the LEGACY path — approve_buyback (a standing Token Metadata
//      transfer delegate) plus CC's own transaction template. The endgame path
//      is the in-program `buyback_pnft` / `buyback_core`, where no delegate
//      exists at all; see the client contract on `buyback_pnft` in lib.rs.
//   2. Step [3] BLIND-SIGNS: it does `Transaction.from(CC's bytes)` and
//      `partialSign` without decoding a single instruction, three steps after
//      handing that same key a transfer delegate over the prize. The real
//      client MUST decode every instruction, display the price from the
//      instruction ARGUMENT (not the API quote), and refuse any transaction
//      carrying an instruction of this program other than the one buyback it
//      is showing. On devnet the blast radius is hidden because `player` is
//      simultaneously cold_owner and hot_delegate — on mainnet it is not.
//
// Full buyback e2e for a vault-held pNFT prize:
//   approve_buyback (hot becomes the TM transfer delegate, slot occupied)
//   → CC /api/buyback with transferAuthority (no-closeAccount template — the
//     API key is scoped to it) → countersign with hot → submit via own RPC
//   → verify NFT left + USDC refund landed in the vault
//   → clear_buyback_slot (proof: the exact emptied account)
//   → close_token_account (cold; rent → cold owner).
//   NODE_PATH=$REPO/node_modules node scripts/devnet/vault_buyback.js
const { load, anchor, TOKEN_PROG } = require("./common");
const { PublicKey, Transaction } = require("@solana/web3.js");
const { getAssociatedTokenAddressSync, getAccount } = require("@solana/spl-token");

// The default is the historical 28.07 run and is SPENT: this script itself
// closed that ATA in step [6], so a re-run without NFT= aborts on nft_token
// with Anchor 3012 AccountNotInitialized. Pass NFT=<mint> for a live prize.
const NFT = new PublicKey(process.env.NFT || "FbJ1qBixuNMcGhXQ5nwvxwFzqnt457p7hrD9M8mCrPzJ");
// Scoped CC dev key: /api/buyback built WITHOUT the closeAccount ix.
const BUYBACK_KEY = process.env.BUYBACK_KEY;
if (!BUYBACK_KEY) throw new Error("set BUYBACK_KEY (scoped buyback key from CC)");
const TM = new PublicKey("metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s");
const AUTH_RULES_PROG = new PublicKey("auth9SigNpDKz4sJJ1DfCTuZrZNSAgh9sFD3rboVmgg");
const RULE_SET = new PublicKey("eBJLFYPxJmMGKuFwpDWkzxZeUrad92kZRC5BJLpzyT9");
const SYSVAR_IX = new PublicKey("Sysvar1nstructions1111111111111111111111111");

const pda = (seeds, prog) => PublicKey.findProgramAddressSync(seeds, prog)[0];

async function main() {
  const { player, conn, program, configPda, vaultPda, vaultUsdc } = load();
  const nftToken = getAssociatedTokenAddressSync(NFT, vaultPda, true);
  const metadata = pda([Buffer.from("metadata"), TM.toBuffer(), NFT.toBuffer()], TM);
  const edition = pda([Buffer.from("metadata"), TM.toBuffer(), NFT.toBuffer(), Buffer.from("edition")], TM);
  const tokenRecord = pda(
    [Buffer.from("metadata"), TM.toBuffer(), NFT.toBuffer(), Buffer.from("token_record"), nftToken.toBuffer()], TM);

  const v0 = await program.account.vault.fetch(vaultPda);
  const usdcBefore = Number((await getAccount(conn, vaultUsdc)).amount) / 1e6;
  console.log(`vault USDC before: ${usdcBefore}; slot: ${v0.liveBuybackMint}; nftToken: ${nftToken.toBase58()}`);

  console.log("[1] approve_buyback (hot -> TM transfer delegate)");
  const sig1 = await program.methods.approveBuyback().accountsPartial({
    config: configPda, vault: vaultPda, hotDelegate: player.publicKey,
    nftMint: NFT, nftToken, metadata, edition, tokenRecord,
    authorizationRules: RULE_SET, authorizationRulesProgram: AUTH_RULES_PROG,
    tokenMetadataProgram: TM, sysvarInstructions: SYSVAR_IX,
    tokenProgram: TOKEN_PROG, systemProgram: anchor.web3.SystemProgram.programId,
  }).rpc();
  console.log("  sig:", sig1);

  console.log("[2] /api/buyback (scoped key) -> countersign -> submit via own RPC");
  const r = await fetch("https://dev-gacha.collectorcrypt.com/api/buyback", {
    method: "POST",
    headers: { "Content-Type": "application/json", "x-api-key": BUYBACK_KEY },
    body: JSON.stringify({
      playerAddress: vaultPda.toBase58(), nftAddress: NFT.toBase58(),
      altRecipient: vaultPda.toBase58(), transferAuthority: player.publicKey.toBase58(),
    }),
  });
  const j = await r.json();
  if (!j.serializedTransaction) { console.log("  buyback build failed:", JSON.stringify(j).slice(0, 400)); return; }
  const tx = Transaction.from(Buffer.from(j.serializedTransaction, "base64"));
  tx.partialSign(player);
  const sig2 = await conn.sendRawTransaction(tx.serialize());
  await conn.confirmTransaction(sig2, "confirmed");
  console.log("  buyback sig:", sig2);

  const nftAfter = Number((await getAccount(conn, nftToken)).amount);
  const usdcAfter = Number((await getAccount(conn, vaultUsdc)).amount) / 1e6;
  console.log(`[3] NFT amount in vault: ${nftAfter}; vault USDC: ${usdcBefore} -> ${usdcAfter}`);

  console.log("[4] clear_buyback_slot (proof: the exact emptied account)");
  const sig3 = await program.methods.clearBuybackSlot().accountsPartial({
    vault: vaultPda, authority: player.publicKey, nftToken,
  }).rpc();
  console.log("  sig:", sig3);

  console.log("[5] close_token_account (cold; rent -> cold owner)");
  const sig4 = await program.methods.closeTokenAccount().accountsPartial({
    vault: vaultPda, coldOwner: player.publicKey, token: nftToken, tokenProgram: TOKEN_PROG,
  }).rpc();
  console.log("  sig:", sig4);

  const v1 = await program.account.vault.fetch(vaultPda);
  console.log(`slot after: mint=${v1.liveBuybackMint} token=${v1.liveBuybackToken}`);
  console.log(">>> BUYBACK E2E COMPLETE: prize sold to CC, refund in the vault, slot free, ATA closed.");
}
main().catch((e) => { console.error(e); process.exit(1); });
