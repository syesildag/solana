#!/usr/bin/env node
/*
 * token_safety.js — arb-specific mint screening.
 *
 * WHY (different from the momentum scanner's checks): inside an arb CYCLE, capital sits
 * in the intermediate token between legs. A live freeze authority can freeze that leg,
 * and a Token-2022 transfer hook can make the second leg fail — both strand funds. These
 * risks do not exist for a pricing-only watcher, so they are screened here, not in
 * scan_tokens.js.
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
