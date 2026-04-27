"use strict";
const https = require('https');

const RPC = process.env.RPC_URL || 'https://api.mainnet-beta.solana.com';

function rpc(method, params) {
  return new Promise((res, rej) => {
    const body = JSON.stringify({ jsonrpc: '2.0', id: 1, method, params });
    const url = new URL(RPC);
    const req = https.request(
      { hostname: url.hostname, path: url.pathname, method: 'POST',
        headers: { 'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(body) } },
      r => { const c = []; r.on('data', d => c.push(d)); r.on('end', () => res(JSON.parse(Buffer.concat(c).toString()))); r.on('error', rej); }
    );
    req.write(body); req.end();
  });
}

const BASE58 = '123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz';
function b58enc(b) {
  const d = [0];
  for (const x of b) { let c = x; for (let i = 0; i < d.length; i++) { c += d[i] << 8; d[i] = c % 58; c = Math.floor(c / 58); } while (c > 0) { d.push(c % 58); c = Math.floor(c / 58); } }
  let r = ''; for (const x of b) { if (x !== 0) break; r += '1'; }
  return r + d.reverse().map(x => BASE58[x]).join('');
}

// Highest-TVL DAMM v1 SOL/MSOL pool from amm.meteora.ag API
const POOL = 'HcjZvfeSNJbNkfLD4eEcRBr96AD3w1GpmMppaeRZf7ur';
const SOL  = 'So11111111111111111111111111111111111111112';
const MSOL = 'mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So';

rpc('getAccountInfo', [POOL, { encoding: 'base64' }]).then(async r => {
  if (!r.result || !r.result.value) { console.log('pool not found'); return; }
  const data = Buffer.from(r.result.value.data[0], 'base64');
  console.log('owner:', r.result.value.owner, 'size:', data.length);

  // Print all Pubkey-aligned fields (every 32 bytes starting at offset 8)
  console.log('\nAll Pubkey-aligned fields:');
  for (let off = 8; off <= data.length - 32; off += 32) {
    console.log(`  off=${off}  ${b58enc(data.slice(off, off + 32))}`);
  }

  // Specifically check offsets 104 and 136 (a_vault, b_vault per meteora.rs docs)
  const aVault = b58enc(data.slice(104, 136));
  const bVault = b58enc(data.slice(136, 168));
  console.log('\na_vault (off 104):', aVault);
  console.log('b_vault (off 136):', bVault);

  // Check if a_vault and b_vault are SPL token accounts or Meteora vault accounts
  const res = await rpc('getMultipleAccounts', [[aVault, bVault], { encoding: 'base64' }]);
  const splOwner = 'TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA';
  (res.result.value || []).forEach((acc, i) => {
    if (!acc) { console.log(`  vault[${i}] not found`); return; }
    console.log(`  vault[${i}] owner=${acc.owner} ${acc.owner === splOwner ? 'SPL TOKEN ACCOUNT' : '(not SPL)'} size=${Buffer.from(acc.data[0],'base64').length}`);
    if (acc.owner !== splOwner) {
      // It's a Meteora vault — find the token_vault inside it
      const vd = Buffer.from(acc.data[0], 'base64');
      console.log(`    vault account size=${vd.length}, scanning for SPL token accounts...`);
      // token_vault in Meteora vault is at a specific offset — print all pubkey fields
      for (let off = 8; off <= Math.min(vd.length - 32, 300); off += 32) {
        console.log(`    inner off=${off}  ${b58enc(vd.slice(off, off + 32))}`);
      }
    }
  });
}).catch(e => console.error(e));
