// Shared devnet wiring for the CC dev-gacha integration scripts.
// The player key (keys/devnet-player.json) is a VALUELESS devnet test key.
const fs = require("fs");
const path = require("path");
const anchor = require("@coral-xyz/anchor");
const { Connection, Keypair, PublicKey } = require("@solana/web3.js");
const { getAssociatedTokenAddressSync } = require("@solana/spl-token");

const REPO = path.join(__dirname, "..", "..");
const API = "https://dev-gacha.collectorcrypt.com/api";
// CC DEV-environment API key (sets our memo slug); scoped to the dev gacha
// only, no funds behind it. Override for other environments — never hardcode
// a prod key here.
// Checked lazily in post()/get(): only the CC API calls need the key — the
// on-chain-only scripts (vault_buyback_atomic.js) must run without it.
const KEY = process.env.CC_API_KEY;
const requireKey = () => {
  if (!KEY) throw new Error("set CC_API_KEY (dev API key from CC)");
  return KEY;
};
const RPC = "https://api.devnet.solana.com";

const USDC = new PublicKey("Gh9ZwEmdLJ8DscKNTkTqPbNwLNNBjuSzaG9Vp2KGtKJr"); // CC dev-USDC (NOT Circle 4zMMC9…)
const GACHA_WALLET = new PublicKey("A4ahkivAG4NoZAE8Sy4qv8nn2DU9yoXRQcttuCeGtTJv");
const GACHA_USDC = new PublicKey("9ZSgA3PMjeAU8K6CWzgJ8oDnhZwwSgHZu6KP3X95t7Jq"); // = ATA(gacha wallet, dev-USDC)
const MEMO_PROG = new PublicKey("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");
const TOKEN_PROG = new PublicKey("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
// Owner program of a Metaplex Core prize: Core assets carry no mint and no
// token account, so an ATA derivation is meaningless for them — the owner
// program is what tells the two prize standards apart.
const MPL_CORE_PROG = new PublicKey("CoREENxT6tW1HoK8ypY1SxRMZTcVPm7R94rH4PZNhX7d");
// Machine and price are overridable: PACK=pokemon_50 AMOUNT=50000000 node ...
// (dev stock rotates; a machine reports "empty" when a tier < lowThreshold).
const PACK = process.env.PACK || "pokemon_250";
const AMOUNT = Number(process.env.AMOUNT || 250_000_000);

function load() {
  const player = Keypair.fromSecretKey(Uint8Array.from(
    JSON.parse(fs.readFileSync(path.join(REPO, "keys", "devnet-player.json")))));
  const conn = new Connection(RPC, "confirmed");
  const provider = new anchor.AnchorProvider(conn, new anchor.Wallet(player), { commitment: "confirmed" });
  anchor.setProvider(provider);
  const idl = JSON.parse(fs.readFileSync(path.join(REPO, "target", "idl", "tangem_gacha_vault.json")));
  const program = new anchor.Program(idl, provider);
  const [configPda] = PublicKey.findProgramAddressSync([Buffer.from("config")], program.programId);
  const [vaultPda] = PublicKey.findProgramAddressSync(
    [Buffer.from("vault"), player.publicKey.toBuffer()], program.programId);
  const vaultUsdc = getAssociatedTokenAddressSync(USDC, vaultPda, true);
  const playerUsdc = getAssociatedTokenAddressSync(USDC, player.publicKey);
  return { player, conn, provider, program, configPda, vaultPda, vaultUsdc, playerUsdc, feeUsdc: playerUsdc };
}

const post = (ep, body) => fetch(`${API}/${ep}`, {
  method: "POST",
  headers: { "Content-Type": "application/json", "x-api-key": requireKey() },
  body: JSON.stringify(body),
}).then(async (r) => ({ status: r.status, json: await r.json().catch(() => ({})) }));
const get = (ep) => fetch(`${API}/${ep}`, { headers: { "x-api-key": requireKey() } }).then((r) => r.json());
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

module.exports = {
  REPO, API, KEY, RPC, USDC, GACHA_WALLET, GACHA_USDC, MEMO_PROG, TOKEN_PROG, MPL_CORE_PROG,
  PACK, AMOUNT,
  load, post, get, sleep, anchor,
};
