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

// Vault IDL Borsh layout (after 8-byte Anchor discriminator):
// offset 8:  enabled (u8)                = 1 byte
// offset 9:  bumps.vaultBump (u8)         = 1 byte
// offset 10: bumps.tokenVaultBump (u8)    = 1 byte
// offset 11: totalAmount (u64)            = 8 bytes → 11-18
// offset 19: tokenVault (Pubkey)          = 32 bytes → 19-50  ← THIS IS THE SPL TOKEN ACCOUNT
// offset 51: feeVault (Pubkey)            = 32 bytes → 51-82
// offset 83: tokenMint (Pubkey)           = 32 bytes → 83-114
// offset 115: lpMint (Pubkey)             = 32 bytes → 115-146
// offset 147: strategies ([Pubkey;30])    = 960 bytes

const vaultAccounts = [
  'FERjPVNEa7Udq8CEv68h6tPL46Tq7ieE49HrE2wea3XT', // a_vault (SOL)
  '8p1VKP45hhqq5iZG5fNGoi7ucme8nFLeChoDWNy7rWFm', // b_vault (MSOL)
];

const splOwner = 'TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA';

rpc('getMultipleAccounts', [vaultAccounts, { encoding: 'base64' }]).then(async r => {
  const tokenVaults = [];
  r.result.value.forEach((acc, i) => {
    if (!acc) { console.log(vaultAccounts[i].slice(0, 8), 'not found'); return; }
    const data = Buffer.from(acc.data[0], 'base64');
    console.log(`vault[${i}] owner=${acc.owner.slice(0, 8)} size=${data.length}`);
    const tokenVault = b58enc(data.slice(19, 51));
    const feeVault = b58enc(data.slice(51, 83));
    const tokenMint = b58enc(data.slice(83, 115));
    const lpMint = b58enc(data.slice(115, 147));
    const totalAmount = data.readBigUInt64LE(11);
    console.log(`  totalAmount=${totalAmount}`);
    console.log(`  tokenVault (off 19): ${tokenVault}`);
    console.log(`  feeVault   (off 51): ${feeVault}`);
    console.log(`  tokenMint  (off 83): ${tokenMint}`);
    console.log(`  lpMint     (off115): ${lpMint}`);
    tokenVaults.push(tokenVault);
  });

  // Now verify tokenVaults are SPL token accounts
  const check = await rpc('getMultipleAccounts', [tokenVaults, { encoding: 'base64' }]);
  check.result.value.forEach((acc, i) => {
    if (!acc) { console.log('\ntokenVault[' + i + '] NOT FOUND'); return; }
    const data = Buffer.from(acc.data[0], 'base64');
    const isSpl = acc.owner === splOwner;
    if (isSpl && data.length >= 72) {
      const mint = b58enc(data.slice(0, 32));
      const amount = data.readBigUInt64LE(64);
      console.log(`\ntokenVault[${i}] SPL ✓  mint=${mint}  amount=${amount}`);
    } else {
      console.log(`\ntokenVault[${i}] NOT SPL (owner=${acc.owner})`);
    }
  });
}).catch(e => console.error(e));
