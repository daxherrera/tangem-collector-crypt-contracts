// E2E spin: pay for a CC gacha spin FROM THE VAULT via open_pack
// (inner CPI transfer + client-added top-level memo), prize to the vault PDA.
// NB: this script follows the CURRENT IDL; if the devnet deployment predates
// it (open_pack signature changed), redeploy before running.
//   NODE_PATH=$REPO/node_modules node scripts/devnet/vault_spin.js
const {
  load, post, get, sleep, anchor, USDC, GACHA_USDC, MEMO_PROG, TOKEN_PROG, MPL_CORE_PROG,
  PACK, AMOUNT,
} = require("./common");
const { PublicKey, Transaction, TransactionInstruction } = require("@solana/web3.js");
const { getAssociatedTokenAddressSync, getAccount, createTransferCheckedInstruction } = require("@solana/spl-token");

async function main() {
  const { player, conn, provider, program, configPda, vaultPda, vaultUsdc, playerUsdc, feeUsdc } = load();

  // top up the vault to >= 252.5 + headroom
  let vBal = Number((await getAccount(conn, vaultUsdc)).amount) / 1e6;
  if (vBal < 260) {
    const need = BigInt(Math.ceil((260 - vBal) * 1e6));
    const tx = new Transaction().add(
      createTransferCheckedInstruction(playerUsdc, USDC, vaultUsdc, player.publicKey, need, 6));
    await provider.sendAndConfirm(tx);
    vBal = Number((await getAccount(conn, vaultUsdc)).amount) / 1e6;
  }
  console.log(`vault USDC before: ${vBal}`);

  console.log(`[1] generatePack ${PACK}: playerAddress=player, altPlayerAddress=vault PDA`);
  const gp = await post("generatePack", {
    playerAddress: player.publicKey.toBase58(),
    packType: PACK,
    altPlayerAddress: vaultPda.toBase58(),
  });
  if (!gp.json.transaction) { console.log("  failed:", JSON.stringify(gp.json)); return; }
  const memo = gp.json.memo;
  const onchainMemo = `${memo}:open`;
  console.log(`  memo=${memo} (discarding CC's tx; paying from the VAULT via open_pack CPI)`);

  const memoIx = new TransactionInstruction({ programId: MEMO_PROG, keys: [], data: Buffer.from(onchainMemo, "utf8") });
  const openPackIx = await program.methods
    .openPack(new anchor.BN(AMOUNT), onchainMemo)
    .accountsPartial({
      config: configPda, vault: vaultPda, hotDelegate: player.publicKey,
      usdcMint: USDC, vaultUsdc, gachaUsdc: GACHA_USDC, feeUsdc,
      tokenProgram: TOKEN_PROG,
    })
    .instruction();
  const tx = new Transaction().add(memoIx, openPackIx);
  tx.feePayer = player.publicKey;
  tx.recentBlockhash = (await conn.getLatestBlockhash()).blockhash;
  tx.sign(player);
  const sig = await conn.sendRawTransaction(tx.serialize());
  await conn.confirmTransaction(sig, "confirmed");
  console.log(`[2] open_pack sig: ${sig}`);
  console.log(`  vault USDC after: ${Number((await getAccount(conn, vaultUsdc)).amount) / 1e6}`);

  console.log(`[3] openPack {memo} — waiting for CC to credit the spin`);
  let opened;
  for (let i = 0; i < 25; i++) {
    const op = await post("openPack", { memo });
    if (op.json.success && op.json.nft_address) { opened = op.json; break; }
    console.log(`  ...${op.json.code || op.json.error || JSON.stringify(op.json)}`);
    await sleep(3000);
  }
  if (!opened) {
    console.log(`\n>>> NOT credited — the webhook did not match this payment (memo + treasury transfer).`);
    const st = await get(`pack/status?memo=${memo}`);
    console.log("    pack/status.pack:", JSON.stringify(st.pack).slice(0, 300));
    return;
  }
  console.log(`  OPENED: nft=${opened.nft_address} rarity=${opened.rarity}`);

  // Prizes come in two standards and the check has to know which. A Core
  // AssetV1 has no mint and no token account — the owner lives inside the
  // asset, whose account is owned by the mpl-core program — so deriving an ATA
  // for it yields a junk address that never resolves. Decide by owner program.
  const asset = new PublicKey(opened.nft_address);
  let delivered = false;
  for (let i = 0; i < 10 && !delivered; i++) {
    try {
      const info = await conn.getAccountInfo(asset);
      if (info && info.owner.equals(MPL_CORE_PROG)) {
        console.log(`  prize in vault: Metaplex Core asset ${asset.toBase58()}`);
        delivered = true;
      } else if (info) {
        const acc = await getAccount(conn, getAssociatedTokenAddressSync(asset, vaultPda, true));
        console.log(`  prize in vault: pNFT amount=${acc.amount} frozen=${acc.isFrozen}`);
        delivered = acc.amount > 0n;
      }
    } catch (e) {
      console.log(`  ...prize not visible yet: ${e.message}`);
    }
    if (!delivered) await sleep(3000);
  }

  if (!delivered) {
    console.log(`\n>>> PAID AND CREDITED, BUT PRIZE NOT SEEN on the vault after ~30s.`);
    console.log(`    Check ${asset.toBase58()} by hand — the USDC has already left the vault.`);
    process.exit(1);
  }
  console.log(`\n>>> SPIN OK: CPI payment from the vault credited by CC, prize in the vault.`);
}
main().catch((e) => { console.error(e); process.exit(1); });
