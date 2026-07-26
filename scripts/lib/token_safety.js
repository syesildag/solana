#!/usr/bin/env node
/*
 * token_safety.js — mint trap screening for anyone who HOLDS the token, however briefly.
 *
 * WHY: inside an arb CYCLE capital sits in the intermediate token between legs, and a live
 * freeze authority (freeze the leg), a Token-2022 transfer hook (block the second leg), or a
 * frozen default account state all strand it. The SAME traps hit a momentum position that is
 * bought and later sold — a hook blocks the exit, a frozen state traps the fill. So this is
 * consumed by BOTH the arb scanner (scan_arb_pools) and the momentum discovery (scan_tokens),
 * not just the arb path. (A purely pricing-only consumer never holds, so it needs none of it.)
 */
"use strict";
const https = require("https");
const http = require("http");

/** Pure: classify a parsed mint account. `info` = data.parsed.info, or null if absent. */
function classifyMintSafety(info) {
  const reasons = [];
  if (!info) return { safe: false, reasons: ["mint account not found or unparseable"] };
  if (info.freezeAuthority) {
    reasons.push(`freeze authority enabled (${info.freezeAuthority}) — a leg can be frozen mid-cycle`);
  }
  const hook = (info.extensions || []).find(
    (e) => e.extension === "transferHook" && e.state && e.state.programId,
  );
  if (hook) {
    reasons.push(`token-2022 transfer hook ${hook.state.programId} — can block the second leg`);
  }
  const frozenDefault = (info.extensions || []).find(
    (e) => e.extension === "defaultAccountState" && e.state && e.state.accountState === "frozen",
  );
  if (frozenDefault) {
    reasons.push("token-2022 defaultAccountState=frozen — accounts created frozen, capital trapped");
  }
  return { safe: reasons.length === 0, reasons };
}

function rpc(rpcUrl, method, params) {
  return new Promise((resolve, reject) => {
    const mod = rpcUrl.startsWith("https") ? https : http;
    const req = mod.request(rpcUrl, { method: "POST", headers: { "content-type": "application/json" } }, (res) => {
      let buf = "";
      res.on("data", (c) => (buf += c));
      res.on("end", () => {
        try { resolve(JSON.parse(buf)); } catch (e) { reject(new Error(`bad RPC response: ${buf.slice(0, 80)}`)); }
      });
    });
    req.on("error", reject);
    req.setTimeout(10000, () => req.destroy(new Error("rpc timeout")));
    req.end(JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }));
  });
}

/** Fetch + classify many mints. Batches of 100 (getMultipleAccounts limit). */
async function fetchMintSafety(rpcUrl, mints) {
  const out = new Map();
  for (let i = 0; i < mints.length; i += 100) {
    const batch = mints.slice(i, i + 100);
    const res = await rpc(rpcUrl, "getMultipleAccounts", [batch, { encoding: "jsonParsed" }]);
    const values = (res.result && res.result.value) || [];
    batch.forEach((mint, j) => {
      const v = values[j];
      const parsed = v && v.data && v.data.parsed;
      const info = parsed && parsed.type === "mint" ? parsed.info : null;
      out.set(mint, classifyMintSafety(info));
    });
  }
  return out;
}

module.exports = { classifyMintSafety, fetchMintSafety };
