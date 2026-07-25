"use strict";
const { test } = require("node:test");
const assert = require("node:assert");
const { classifyMintSafety } = require("./token_safety");

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
