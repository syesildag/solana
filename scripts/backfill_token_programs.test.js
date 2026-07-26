"use strict";
const { test } = require("node:test");
const assert = require("node:assert");
const { applyTokenPrograms, TOKEN_2022 } = require("./backfill_token_programs");

const ANSEM = "9cRCn9rGT8V2imeM2BaKs13yhMEais3ruM3rPvTGpump";
const USDC = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const SOL = "So11111111111111111111111111111111111111112";
const CLASSIC = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

test("applyTokenPrograms stamps only Token-2022 sides, leaves classic unset, is idempotent", () => {
  const owners = new Map([[ANSEM, TOKEN_2022], [USDC, CLASSIC], [SOL, CLASSIC]]);
  const pools = [
    { id: "ansem-usdc", token_a: ANSEM, token_b: USDC, extra: {} }, // ANSEM side → stamped
    { id: "sol-usdc", token_a: SOL, token_b: USDC, extra: {} },     // both classic → untouched
  ];
  assert.equal(applyTokenPrograms(pools, owners), 1, "only the ANSEM side is stamped");
  assert.equal(pools[0].extra.token_program_a, TOKEN_2022, "ANSEM (token_a) → Token-2022");
  assert.equal(pools[0].extra.token_program_b, undefined, "USDC (classic) left unset — Rust defaults to classic");
  assert.equal(pools[1].extra.token_program_a, undefined, "SOL classic left unset");
  assert.equal(applyTokenPrograms(pools, owners), 0, "re-run stamps nothing new (idempotent)");
});

test("applyTokenPrograms creates extra when absent and stamps token_b too", () => {
  const owners = new Map([[USDC, CLASSIC], [ANSEM, TOKEN_2022]]);
  const pools = [{ id: "usdc-ansem", token_a: USDC, token_b: ANSEM }]; // no extra; ANSEM is token_b
  assert.equal(applyTokenPrograms(pools, owners), 1);
  assert.equal(pools[0].extra.token_program_b, TOKEN_2022, "ANSEM (token_b) → Token-2022");
  assert.equal(pools[0].extra.token_program_a, undefined);
});
