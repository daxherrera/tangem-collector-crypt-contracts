import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import {
  Keypair,
  PublicKey,
  SystemProgram,
  LAMPORTS_PER_SOL,
} from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID,
  createMint,
  createAssociatedTokenAccount,
  getAssociatedTokenAddressSync,
  mintTo,
  getAccount,
} from "@solana/spl-token";
import { assert } from "chai";
import { TangemGachaVault } from "../target/types/tangem_gacha_vault";

const BPF_UPGRADEABLE_LOADER = new PublicKey(
  "BPFLoaderUpgradeab1e11111111111111111111111"
);

describe("tangem-gacha-vault", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);
  const program = anchor.workspace
    .TangemGachaVault as Program<TangemGachaVault>;
  const connection = provider.connection;
  const admin = (provider.wallet as anchor.Wallet).payer;

  // Actors
  const coldOwner = Keypair.generate(); // simulates the Tangem card key
  const coldOwner2 = Keypair.generate(); // second vault for daily-cap test
  const hotDelegate = Keypair.generate(); // simulates the phone key
  const gachaWallet = Keypair.generate(); // simulates Collector Crypt treasury
  const feeTreasury = Keypair.generate(); // Tangem fee wallet
  const attacker = Keypair.generate();

  const USDC_DECIMALS = 6;
  const usd = (n: number) => new anchor.BN(n * 10 ** USDC_DECIMALS);

  let usdcMint: PublicKey;
  let gachaUsdc: PublicKey;
  let feeUsdc: PublicKey;
  let attackerUsdc: PublicKey;
  let vaultPda: PublicKey;
  let vault2Pda: PublicKey;
  let configPda: PublicKey;
  let programDataPda: PublicKey;
  let vaultUsdc: PublicKey;
  let vault2Usdc: PublicKey;

  const FEE_BPS = 100; // 1% on top
  const PER_SPIN_CAP = usd(300); // covers a 250 USDC pack + fee
  const DAILY_CAP = usd(1000);

  // On-chain memo = generatePack's "<slug>-<uuid>" + ":open" (appended by our
  // backend; the API field is the bare join key).
  const memo = (uuid: string) => `tangem-${uuid}:open`;

  before(async () => {
    await Promise.all(
      [coldOwner, coldOwner2, hotDelegate, gachaWallet, attacker].map(
        async (kp) => {
          const sig = await connection.requestAirdrop(
            kp.publicKey,
            5 * LAMPORTS_PER_SOL
          );
          const latest = await connection.getLatestBlockhash();
          await connection.confirmTransaction({ signature: sig, ...latest });
        }
      )
    );

    usdcMint = await createMint(
      connection,
      admin,
      admin.publicKey,
      null,
      USDC_DECIMALS
    );
    gachaUsdc = await createAssociatedTokenAccount(
      connection,
      admin,
      usdcMint,
      gachaWallet.publicKey
    );
    feeUsdc = await createAssociatedTokenAccount(
      connection,
      admin,
      usdcMint,
      feeTreasury.publicKey
    );
    attackerUsdc = await createAssociatedTokenAccount(
      connection,
      admin,
      usdcMint,
      attacker.publicKey
    );

    [configPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("config")],
      program.programId
    );
    [programDataPda] = PublicKey.findProgramAddressSync(
      [program.programId.toBuffer()],
      BPF_UPGRADEABLE_LOADER
    );
    [vaultPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault"), coldOwner.publicKey.toBuffer()],
      program.programId
    );
    [vault2Pda] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault"), coldOwner2.publicKey.toBuffer()],
      program.programId
    );
    vaultUsdc = getAssociatedTokenAddressSync(usdcMint, vaultPda, true);
    vault2Usdc = getAssociatedTokenAddressSync(usdcMint, vault2Pda, true);
  });

  it("initializes config (upgrade authority only)", async () => {
    await program.methods
      .initializeConfig(
        gachaWallet.publicKey,
        gachaUsdc,
        feeUsdc,
        FEE_BPS,
        true // buyback delegation on
      )
      .accountsPartial({
        config: configPda,
        usdcMint,
        admin: admin.publicKey,
        program: program.programId,
        programData: programDataPda,
        systemProgram: SystemProgram.programId,
      })
      .rpc();

    const cfg = await program.account.config.fetch(configPda);
    assert.ok(cfg.gachaUsdcAccount.equals(gachaUsdc));
    assert.equal(cfg.feeBps, FEE_BPS);
    assert.isTrue(cfg.allowBuybackDelegation);
  });

  it("initializes a vault (cold signs, hot pays rent) and funds it", async () => {
    await program.methods
      .initVault(hotDelegate.publicKey, PER_SPIN_CAP, DAILY_CAP)
      .accountsPartial({
        vault: vaultPda,
        coldOwner: coldOwner.publicKey,
        payer: hotDelegate.publicKey,
        systemProgram: SystemProgram.programId,
      })
      .signers([coldOwner, hotDelegate])
      .rpc();

    const vault = await program.account.vault.fetch(vaultPda);
    assert.ok(vault.coldOwner.equals(coldOwner.publicKey));
    assert.ok(vault.hotDelegate.equals(hotDelegate.publicKey));
    assert.isNull(vault.liveBuybackMint);

    // Deposit: in production a plain SPL transfer signed by the cold key
    // (one tap). Here we just mint directly to the vault ATA.
    await createAssociatedTokenAccount(
      connection,
      admin,
      usdcMint,
      vaultPda,
      undefined,
      undefined,
      undefined,
      true
    );
    await mintTo(connection, admin, usdcMint, vaultUsdc, admin, 500_000_000); // 500 USDC
  });

  it("open_pack: pays gacha + fee with memo, signed only by hot key", async () => {
    await program.methods
      .openPack(usd(50), memo("11111111-aaaa-bbbb-cccc-000000000001"))
      .accountsPartial({
        config: configPda,
        vault: vaultPda,
        hotDelegate: hotDelegate.publicKey,
        usdcMint,
        vaultUsdc,
        gachaUsdc,
        feeUsdc,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([hotDelegate])
      .rpc();

    const gacha = await getAccount(connection, gachaUsdc);
    const fee = await getAccount(connection, feeUsdc);
    assert.equal(gacha.amount.toString(), usd(50).toString());
    assert.equal(fee.amount.toString(), usd(0.5).toString()); // 1% of 50

    const vault = await program.account.vault.fetch(vaultPda);
    assert.equal(vault.spentToday.toString(), usd(50.5).toString());
  });

  it("open_pack: rejects a non-whitelisted destination", async () => {
    try {
      await program.methods
        .openPack(usd(10), memo("11111111-aaaa-bbbb-cccc-000000000002"))
        .accountsPartial({
          config: configPda,
          vault: vaultPda,
          hotDelegate: hotDelegate.publicKey,
          usdcMint,
          vaultUsdc,
          gachaUsdc: attackerUsdc, // <-- attacker swaps destination
          feeUsdc,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([hotDelegate])
        .rpc();
      assert.fail("should have thrown");
    } catch (e: any) {
      assert.include(e.toString(), "ConstraintAddress");
    }
  });

  it("open_pack: rejects a non-delegate signer", async () => {
    try {
      await program.methods
        .openPack(usd(10), memo("11111111-aaaa-bbbb-cccc-000000000003"))
        .accountsPartial({
          config: configPda,
          vault: vaultPda,
          hotDelegate: attacker.publicKey,
          usdcMint,
          vaultUsdc,
          gachaUsdc,
          feeUsdc,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([attacker])
        .rpc();
      assert.fail("should have thrown");
    } catch (e: any) {
      assert.include(e.toString(), "ConstraintHasOne");
    }
  });

  it("open_pack: per-spin cap is fee-inclusive (fee pushes total over)", async () => {
    // 299 alone fits the 300 cap; 299 + 1% fee = 301.99 must be refused.
    // (The bare above-cap case is covered by caps.rs::per_spin_cap_enforced.)
    try {
      await program.methods
        .openPack(usd(299), memo("11111111-aaaa-bbbb-cccc-000000000004"))
        .accountsPartial({
          config: configPda,
          vault: vaultPda,
          hotDelegate: hotDelegate.publicKey,
          usdcMint,
          vaultUsdc,
          gachaUsdc,
          feeUsdc,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([hotDelegate])
        .rpc();
      assert.fail("should have thrown");
    } catch (e: any) {
      assert.include(e.toString(), "ExceedsPerSpinCap");
    }
  });

  it("open_pack: enforces the daily cap (second vault, tight caps)", async () => {
    await program.methods
      .initVault(hotDelegate.publicKey, usd(100 * 1.01), usd(150))
      .accountsPartial({
        vault: vault2Pda,
        coldOwner: coldOwner2.publicKey,
        payer: hotDelegate.publicKey,
        systemProgram: SystemProgram.programId,
      })
      .signers([coldOwner2, hotDelegate])
      .rpc();
    await createAssociatedTokenAccount(
      connection,
      admin,
      usdcMint,
      vault2Pda,
      undefined,
      undefined,
      undefined,
      true
    );
    await mintTo(connection, admin, usdcMint, vault2Usdc, admin, 300_000_000);

    const spin = (uuid: string, amount: anchor.BN) =>
      program.methods
        .openPack(amount, memo(uuid))
        .accountsPartial({
          config: configPda,
          vault: vault2Pda,
          hotDelegate: hotDelegate.publicKey,
          usdcMint,
          vaultUsdc: vault2Usdc,
          gachaUsdc,
          feeUsdc,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([hotDelegate])
        .rpc();

    // 100 + 1% fee = 101 charged; next 50-spin needs 50.5 more > 150 cap.
    await spin("22222222-aaaa-bbbb-cccc-000000000001", usd(100));
    try {
      await spin("22222222-aaaa-bbbb-cccc-000000000002", usd(50));
      assert.fail("should have thrown");
    } catch (e: any) {
      assert.include(e.toString(), "ExceedsDailyCap");
    }
  });

  // approve_buyback / revoke_buyback / clear_buyback_slot / withdraw_pnft
  // operate on Metaplex programmable NFTs (frozen token accounts) and need
  // the real Token Metadata + auth-rules programs — they are covered by the
  // litesvm suite (programs/tangem-gacha-vault/tests/pnft.rs) against
  // mainnet-dumped fixtures; withdraw_core likewise in
  // programs/tangem-gacha-vault/tests/core.rs. This
  // hermetic TS suite covers the SPL-level logic (auth, caps, config).

  it("close_vault: closes a clean vault, rent returns to the cold owner", async () => {
    // The VaultNotClean gate (live pNFT buyback delegate) is exercised in the
    // litesvm suite (pnft.rs close_vault_rejected_while_buyback_slot_live);
    // the hermetic TS suite covers the happy path on the idle second vault.
    await program.methods
      .closeVault()
      .accountsPartial({ vault: vault2Pda, coldOwner: coldOwner2.publicKey })
      .signers([coldOwner2])
      .rpc();
    assert.isNull(await program.account.vault.fetchNullable(vault2Pda));
  });

  it("withdraw_token: hot key cannot withdraw", async () => {
    try {
      await program.methods
        .withdrawToken(usd(1))
        .accountsPartial({
          vault: vaultPda,
          coldOwner: hotDelegate.publicKey, // wrong authority
          source: vaultUsdc,
          mint: usdcMint,
          destination: attackerUsdc,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([hotDelegate])
        .rpc();
      assert.fail("should have thrown");
    } catch (e: any) {
      // vault PDA seeds derive from cold_owner, so a wrong signer breaks them
      assert.include(e.toString(), "ConstraintSeeds");
    }
  });

  it("withdraw_token: cold key withdraws anywhere", async () => {
    const coldUsdc = await createAssociatedTokenAccount(
      connection,
      admin,
      usdcMint,
      coldOwner.publicKey
    );
    await program.methods
      .withdrawToken(usd(100))
      .accountsPartial({
        vault: vaultPda,
        coldOwner: coldOwner.publicKey,
        source: vaultUsdc,
        mint: usdcMint,
        destination: coldUsdc,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([coldOwner])
      .rpc();
    const acc = await getAccount(connection, coldUsdc);
    assert.equal(acc.amount.toString(), usd(100).toString());
  });

  it("close_token_account: cold key reclaims rent from an empty vault ATA", async () => {
    // A separate throwaway mint stands in for a GC'able empty account.
    const junkMint = await createMint(connection, admin, admin.publicKey, null, 0);
    const junkAta = await createAssociatedTokenAccount(
      connection,
      admin,
      junkMint,
      vaultPda,
      undefined,
      undefined,
      undefined,
      true
    );

    const before = await connection.getBalance(coldOwner.publicKey);
    await program.methods
      .closeTokenAccount()
      .accountsPartial({
        vault: vaultPda,
        coldOwner: coldOwner.publicKey,
        token: junkAta,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([coldOwner])
      .rpc();

    assert.isNull(await connection.getAccountInfo(junkAta));
    const after = await connection.getBalance(coldOwner.publicKey);
    assert.isAbove(after, before); // rent came back (minus tx fee paid by provider)
  });

  it("withdraw_sol: cold key sweeps excess lamports, rent floor stays", async () => {
    const topUp = anchor.web3.SystemProgram.transfer({
      fromPubkey: admin.publicKey,
      toPubkey: vaultPda,
      lamports: 0.1 * LAMPORTS_PER_SOL,
    });
    await provider.sendAndConfirm(new anchor.web3.Transaction().add(topUp));

    await program.methods
      .withdrawSol(new anchor.BN(0.1 * LAMPORTS_PER_SOL))
      .accountsPartial({
        vault: vaultPda,
        coldOwner: coldOwner.publicKey,
        recipient: coldOwner.publicKey,
      })
      .signers([coldOwner])
      .rpc();

    try {
      await program.methods
        .withdrawSol(new anchor.BN(LAMPORTS_PER_SOL)) // more than remains above rent
        .accountsPartial({
          vault: vaultPda,
          coldOwner: coldOwner.publicKey,
          recipient: coldOwner.publicKey,
        })
        .signers([coldOwner])
        .rpc();
      assert.fail("should have thrown");
    } catch (e: any) {
      assert.include(e.toString(), "InsufficientSol");
    }
  });

  it("withdraw_sol: rejects a zero amount and the vault as its own recipient", async () => {
    try {
      await program.methods
        .withdrawSol(new anchor.BN(0))
        .accountsPartial({
          vault: vaultPda,
          coldOwner: coldOwner.publicKey,
          recipient: coldOwner.publicKey,
        })
        .signers([coldOwner])
        .rpc();
      assert.fail("should have thrown");
    } catch (e: any) {
      assert.include(e.toString(), "ZeroAmount");
    }

    // Aliasing the vault into the recipient slot would net out to a no-op and
    // report success, so it is refused outright.
    try {
      await program.methods
        .withdrawSol(new anchor.BN(1))
        .accountsPartial({
          vault: vaultPda,
          coldOwner: coldOwner.publicKey,
          recipient: vaultPda,
        })
        .signers([coldOwner])
        .rpc();
      assert.fail("should have thrown");
    } catch (e: any) {
      assert.include(e.toString(), "Unauthorized");
    }
  });

  it("pause blocks open_pack", async () => {
    await program.methods
      .updateConfig(null, null, null, null, null, true, null)
      .accountsPartial({ config: configPda, admin: admin.publicKey })
      .rpc();
    try {
      await program.methods
        .openPack(usd(10), memo("11111111-aaaa-bbbb-cccc-000000000005"))
        .accountsPartial({
          config: configPda,
          vault: vaultPda,
          hotDelegate: hotDelegate.publicKey,
          usdcMint,
          vaultUsdc,
          gachaUsdc,
          feeUsdc,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([hotDelegate])
        .rpc();
      assert.fail("should have thrown");
    } catch (e: any) {
      assert.include(e.toString(), "Paused");
    }
  });

  it("update_config: non-admin is rejected", async () => {
    try {
      await program.methods
        .updateConfig(null, null, null, null, null, false, null)
        .accountsPartial({ config: configPda, admin: attacker.publicKey })
        .signers([attacker])
        .rpc();
      assert.fail("should have thrown");
    } catch (e: any) {
      assert.include(e.toString(), "ConstraintHasOne");
    }
  });

  it("update_config: fee above MAX_FEE_BPS is rejected", async () => {
    try {
      await program.methods
        .updateConfig(null, null, null, null, 5000, null, null)
        .accountsPartial({ config: configPda, admin: admin.publicKey })
        .rpc();
      assert.fail("should have thrown");
    } catch (e: any) {
      assert.include(e.toString(), "InvalidFeeBps");
    }
  });
});
