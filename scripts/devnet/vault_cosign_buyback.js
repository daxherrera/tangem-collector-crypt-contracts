// The Tangem half of the buyback submission protocol: take a CC-built,
// partially-signed buyback transaction, validate it against the co-signing
// whitelist (the same checks the phone performs before signing), display the
// price from the INSTRUCTION ARGUMENT, co-sign with the hot key and hand the
// transaction back (or submit it).
//
// Whitelist enforced here (must stay in sync with the spec, §9):
//   - exactly ONE instruction of this program, and it is buyback_core or
//     buyback_pnft; the asset it moves is reported for display;
//   - at most one SetComputeUnitLimit, one SetComputeUnitPrice, one SPL Memo;
//   - optionally the idempotent creation of the vault's canonical USDC ATA
//     (payer = cc_authority, owner = the vault, mint from the config);
//   - nothing else; and no instruction other than the buyback itself may use
//     the hot key as a signer or as a source of funds;
//   - the fee payer is NOT the hot key (CC pays the network fee).
//
// Run: TX=<base64> node scripts/devnet/vault_cosign_buyback.js        (print)
//      TX=<base64> SUBMIT=1 node scripts/devnet/vault_cosign_buyback.js
//      TX_FILE=path/to/tx.b64 … — read the transaction from a file instead.
// The hot key defaults to keys/devnet-player.json; override with HOT_KEY.
// No CC_API_KEY needed.
const fs = require("fs");
const path = require("path");
const { Keypair, PublicKey, Transaction } = require("@solana/web3.js");
const { ASSOCIATED_TOKEN_PROGRAM_ID } = require("@solana/spl-token");
const { REPO, USDC, MEMO_PROG, TOKEN_PROG, load } = require("./common");

const COMPUTE_BUDGET_PROG = new PublicKey("ComputeBudget111111111111111111111111111111");
// sha256("global:buyback_core")[0..8] / sha256("global:buyback_pnft")[0..8]
const DISC_CORE = Buffer.from([1, 108, 17, 204, 52, 40, 164, 144]);
const DISC_PNFT = Buffer.from([229, 155, 236, 186, 92, 43, 99, 54]);

// Positions in the account lists (0-based), per the IDL.
const SLOT = {
  core: { hot: 2, ccAuthority: 3, destinationOwner: 4, asset: 8 },
  pnft: { hot: 2, ccAuthority: 3, destinationOwner: 4, asset: 8 }, // 8 = nft_mint
};

function decodeBuybackArgs(data) {
  // 8-byte discriminator ‖ price: u64 LE ‖ memo: u32 LE length + utf8
  const price = data.readBigUInt64LE(8);
  const memoLen = data.readUInt32LE(16);
  const memo = data.subarray(20, 20 + memoLen).toString("utf8");
  return { price, memo };
}

/// Validates the whitelist and returns what the phone would display.
/// Throws with a specific reason on the first violation.
function validateBuybackTx(tx, { hotPubkey, vaultPda, vaultUsdc, programId }) {
  const summary = { kind: null, price: null, memo: null, asset: null, destinationOwner: null };
  let buybacks = 0, cuLimit = 0, cuPrice = 0, memos = 0, ataCreates = 0;

  if (tx.feePayer.equals(hotPubkey)) throw new Error("fee payer must be CC, not the hot key");

  for (const [i, ix] of tx.instructions.entries()) {
    const pid = ix.programId;
    if (pid.equals(programId)) {
      buybacks += 1;
      const disc = ix.data.subarray(0, 8);
      const kind = disc.equals(DISC_CORE) ? "core" : disc.equals(DISC_PNFT) ? "pnft" : null;
      if (!kind) throw new Error(`ix[${i}]: program instruction is not a buyback`);
      const { price, memo } = decodeBuybackArgs(ix.data);
      const slots = SLOT[kind];
      if (!ix.keys[slots.hot].pubkey.equals(hotPubkey)) throw new Error(`ix[${i}]: hot_delegate slot is not our hot key`);
      summary.kind = kind;
      summary.price = price;
      summary.memo = memo;
      summary.asset = ix.keys[slots.asset].pubkey;
      summary.destinationOwner = ix.keys[slots.destinationOwner].pubkey;
      summary.ccAuthority = ix.keys[slots.ccAuthority].pubkey;
    } else if (pid.equals(COMPUTE_BUDGET_PROG)) {
      if (ix.data[0] === 2) cuLimit += 1;
      else if (ix.data[0] === 3) cuPrice += 1;
      else throw new Error(`ix[${i}]: unexpected ComputeBudget variant ${ix.data[0]}`);
    } else if (pid.equals(MEMO_PROG)) {
      memos += 1;
    } else if (pid.equals(ASSOCIATED_TOKEN_PROGRAM_ID)) {
      if (!ix.data.equals(Buffer.from([1]))) throw new Error(`ix[${i}]: only CreateIdempotent is allowed for the ATA program`);
      if (!ix.keys[1].pubkey.equals(vaultUsdc)) throw new Error(`ix[${i}]: ATA creation targets a foreign account`);
      if (!ix.keys[2].pubkey.equals(vaultPda)) throw new Error(`ix[${i}]: ATA owner is not the vault`);
      if (!ix.keys[3].pubkey.equals(USDC)) throw new Error(`ix[${i}]: ATA mint is not the configured USDC`);
      if (ix.keys[0].pubkey.equals(hotPubkey)) throw new Error(`ix[${i}]: the hot key must not pay for the ATA`);
      ataCreates += 1;
    } else {
      throw new Error(`ix[${i}]: program ${pid.toBase58()} is not whitelisted`);
    }
    // The hot key's only role in the WHOLE transaction is signing the buyback:
    // in any other instruction it may appear neither as a signer nor writable
    // (writable elsewhere = a potential source of funds).
    if (!pid.equals(programId)) {
      for (const k of ix.keys) {
        if (k.pubkey.equals(hotPubkey) && (k.isSigner || k.isWritable)) {
          throw new Error(`ix[${i}]: the hot key is used outside the buyback instruction`);
        }
      }
    }
  }

  if (buybacks !== 1) throw new Error(`expected exactly one buyback instruction, got ${buybacks}`);
  if (cuLimit > 1 || cuPrice > 1) throw new Error("more than one ComputeBudget instruction of the same kind");
  if (memos > 1) throw new Error("more than one SPL Memo instruction");
  if (ataCreates > 1) throw new Error("more than one ATA creation");
  return summary;
}

function cosign(tx, hotKeypair) {
  tx.partialSign(hotKeypair);
  return tx;
}

module.exports = { validateBuybackTx, cosign, DISC_CORE, DISC_PNFT };

if (require.main === module) {
  (async () => {
    const b64 = process.env.TX || (process.env.TX_FILE && fs.readFileSync(process.env.TX_FILE, "utf8").trim());
    if (!b64) throw new Error("pass the CC-built transaction via TX=<base64> or TX_FILE=<path>");
    const hot = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(
      fs.readFileSync(process.env.HOT_KEY || path.join(REPO, "keys", "devnet-player.json")))));
    const { conn, program, vaultPda, vaultUsdc } = load();

    const tx = Transaction.from(Buffer.from(b64, "base64"));
    const s = validateBuybackTx(tx, {
      hotPubkey: hot.publicKey, vaultPda, vaultUsdc, programId: program.programId,
    });
    console.log("whitelist: OK");
    console.log(`  buyback_${s.kind}; price: ${Number(s.price) / 1e6} USDC (raw ${s.price})`);
    console.log(`  memo: ${s.memo}`);
    console.log(`  asset: ${s.asset.toBase58()}`);
    console.log(`  destination_owner: ${s.destinationOwner.toBase58()}`);
    console.log(`  cc_authority (signer/payer): ${s.ccAuthority.toBase58()}`);

    cosign(tx, hot);
    const wire = tx.serialize(); // verifies all required signatures are present
    console.log(`co-signed; serialized ${wire.length} bytes`);
    if (process.env.SUBMIT) {
      const sig = await conn.sendRawTransaction(wire);
      const conf = await conn.confirmTransaction(sig, "confirmed");
      if (conf.value.err) throw new Error(`submitted but failed on-chain: ${JSON.stringify(conf.value.err)}`);
      console.log("submitted:", sig);
    } else {
      console.log("co-signed transaction (base64):");
      console.log(wire.toString("base64"));
    }
  })().catch((e) => { console.error("REJECTED:", e.message); process.exit(1); });
}
