"use strict";
const { test } = require("node:test");
const assert = require("node:assert");
const { classifyMintSafety, fetchMintSafety } = require("./token_safety");

const clean = { decimals: 6, mintAuthority: null, freezeAuthority: null };

test("accepts a clean mint", () => {
  const r = classifyMintSafety(clean);
  assert.equal(r.safe, true);
  assert.deepEqual(r.reasons, []);
});

test("rejects a mint with freeze authority enabled", () => {
  const r = classifyMintSafety({ ...clean, freezeAuthority: "Fr33zeAuth11111111111111111111111111111111" });
  assert.equal(r.safe, false);
  assert.match(r.reasons.join(" "), /freeze authority/i);
});

test("rejects a Token-2022 mint with a transfer hook", () => {
  const info = {
    ...clean,
    extensions: [{ extension: "transferHook", state: { programId: "Hook111111111111111111111111111111111111111" } }],
  };
  const r = classifyMintSafety(info);
  assert.equal(r.safe, false);
  assert.match(r.reasons.join(" "), /transfer hook/i);
});

test("allows benign Token-2022 extensions (e.g. metadata pointer)", () => {
  const info = { ...clean, extensions: [{ extension: "metadataPointer", state: {} }] };
  assert.equal(classifyMintSafety(info).safe, true);
});

test("treats a missing mint account as unsafe", () => {
  const r = classifyMintSafety(null);
  assert.equal(r.safe, false);
  assert.match(r.reasons.join(" "), /not found/i);
});

test("mint authority alone does not reject (recorded only)", () => {
  const r = classifyMintSafety({ ...clean, mintAuthority: "Mint111111111111111111111111111111111111111" });
  assert.equal(r.safe, true, "inflatable supply is a momentum concern, not a trapped-capital one");
});

test("rejects a token-2022 mint with defaultAccountState = frozen", () => {
  const info = { decimals: 6, mintAuthority: null, freezeAuthority: null,
    extensions: [{ extension: "defaultAccountState", state: { accountState: "frozen" } }] };
  const r = classifyMintSafety(info);
  assert.equal(r.safe, false);
  assert.match(r.reasons.join(" "), /frozen/i);
});

test("allows defaultAccountState = initialized", () => {
  const info = { decimals: 6, mintAuthority: null, freezeAuthority: null,
    extensions: [{ extension: "defaultAccountState", state: { accountState: "initialized" } }] };
  assert.equal(classifyMintSafety(info).safe, true);
});

// ─── fetchMintSafety RPC-error handling ──────────────────────────────────────
// A rate-limited RPC response ({error:{code:-32429}}) must NOT be classified as
// "mint account not found" — that misled a real debugging session (NEST, 2026-07-26).

const goodValue = { data: { parsed: { type: "mint", info: { decimals: 6, mintAuthority: null, freezeAuthority: null } } } };

test("retries a rate-limited batch and succeeds", async () => {
  let calls = 0;
  const fakeRpc = async () => {
    calls++;
    if (calls <= 2) return { error: { code: -32429, message: "rate limited" } };
    return { result: { value: [goodValue] } };
  };
  const out = await fetchMintSafety("http://unused", ["MintA"], { _rpc: fakeRpc, backoffMs: 0 });
  assert.equal(calls, 3, "should retry until the RPC stops rate-limiting");
  assert.equal(out.get("MintA").safe, true);
});

test("reports an honest reason when RPC stays rate-limited", async () => {
  let calls = 0;
  const fakeRpc = async () => { calls++; return { error: { code: -32429, message: "rate limited" } }; };
  const out = await fetchMintSafety("http://unused", ["MintA"], { _rpc: fakeRpc, retries: 2, backoffMs: 0 });
  assert.equal(calls, 3, "initial attempt + 2 retries");
  const r = out.get("MintA");
  assert.equal(r.safe, false, "must stay fail-closed — an unscreened token is not admitted");
  assert.match(r.reasons.join(" "), /rpc error|rate limited/i);
  assert.doesNotMatch(r.reasons.join(" "), /not found/i, "must not misreport an RPC failure as a missing mint");
});
