#!/usr/bin/env node
"use strict";

const { Connection, PublicKey, Keypair, Transaction, sendAndConfirmTransaction } = require("@solana/web3.js");
const {
  getAssociatedTokenAddressSync,
  createAssociatedTokenAccountInstruction,
  TOKEN_PROGRAM_ID,
  TOKEN_2022_PROGRAM_ID,
} = require("@solana/spl-token");
const fs   = require("fs");
const path = require("path");
const os   = require("os");

const WSOL = "So11111111111111111111111111111111111111112";

async function main() {
  const rpcUrl      = process.env.RPC_URL      || "https://api.mainnet-beta.solana.com";
  const keypairPath = (process.env.WALLET_KEYPAIR_PATH || "~/.config/solana/id.json")
                        .replace(/^~/, os.homedir());
  const poolsPath   = process.env.POOLS_CONFIG_PATH
                        || path.join(__dirname, "../pools.json");

  const wallet = Keypair.fromSecretKey(
    Uint8Array.from(JSON.parse(fs.readFileSync(keypairPath, "utf-8")))
  );
  const connection = new Connection(rpcUrl, "confirmed");
  const pools      = JSON.parse(fs.readFileSync(poolsPath, "utf-8"));

  // Collect unique non-WSOL mints from pools.json
  const mints = new Set();
  for (const pool of pools) {
    if (pool.token_a && pool.token_a !== WSOL) mints.add(pool.token_a);
    if (pool.token_b && pool.token_b !== WSOL) mints.add(pool.token_b);
  }
  console.log(`Found ${mints.size} unique non-WSOL mints across ${pools.length} pools`);

  // Resolve each mint's token program first — the ATA address derivation includes
  // the token program, so a Token-2022 mint (e.g. xStocks) derived/created with the
  // legacy program id checks the wrong address and fails with IncorrectProgramId.
  const mintList = [...mints].map(m => new PublicKey(m));
  const programByMint = new Map();
  for (let i = 0; i < mintList.length; i += 100) {
    const batch = mintList.slice(i, i + 100);
    const infos = await connection.getMultipleAccountsInfo(batch);
    for (let j = 0; j < batch.length; j++) {
      const owner = infos[j]?.owner;
      if (!owner) { console.warn(`  mint ${batch[j].toBase58()} not found on-chain — skipping`); continue; }
      if (!owner.equals(TOKEN_PROGRAM_ID) && !owner.equals(TOKEN_2022_PROGRAM_ID)) {
        console.warn(`  mint ${batch[j].toBase58()} owned by ${owner.toBase58()} (not a token program) — skipping`);
        continue;
      }
      programByMint.set(batch[j].toBase58(), owner);
    }
  }

  // Derive ATA addresses under each mint's own token program
  const ataAccounts = [];
  for (const mint of mintList) {
    const program = programByMint.get(mint.toBase58());
    if (!program) continue;
    const ata = getAssociatedTokenAddressSync(mint, wallet.publicKey, false, program);
    ataAccounts.push({ mint, ata, program });
  }

  // Batch-check existence (100 per getMultipleAccountsInfo call)
  const missing = [];
  for (let i = 0; i < ataAccounts.length; i += 100) {
    const batch = ataAccounts.slice(i, i + 100);
    const infos = await connection.getMultipleAccountsInfo(batch.map(a => a.ata));
    for (let j = 0; j < batch.length; j++) {
      if (!infos[j]) missing.push(batch[j]);
    }
  }

  if (missing.length === 0) {
    console.log(`All ${ataAccounts.length} ATAs already exist — nothing to create.`);
    return;
  }

  console.log(`Creating ${missing.length} missing ATAs (${ataAccounts.length - missing.length} already exist)...`);

  // Create in batches of 10 per transaction
  for (let i = 0; i < missing.length; i += 10) {
    const batch = missing.slice(i, i + 10);
    const { blockhash } = await connection.getLatestBlockhash();
    const tx = new Transaction({ recentBlockhash: blockhash, feePayer: wallet.publicKey });
    for (const { mint, ata, program } of batch) {
      tx.add(createAssociatedTokenAccountInstruction(
        wallet.publicKey, ata, wallet.publicKey, mint, program,
      ));
    }
    const sig = await sendAndConfirmTransaction(connection, tx, [wallet]);
    console.log(`  Batch ${Math.floor(i / 10) + 1}: created ${batch.length} ATAs (sig: ${sig.slice(0, 8)}...)`);
  }

  console.log(`Done: ${missing.length} ATAs created.`);
}

main().catch(err => { console.error(err); process.exit(1); });
