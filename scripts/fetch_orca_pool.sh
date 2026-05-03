#!/usr/bin/env bash
# Fetch an Orca Whirlpool pool's vault accounts, fee_bps, and current tick arrays.
# Usage: ./fetch_orca_pool.sh <pool_pubkey> [rpc_url]
#
# Output: JSON snippet ready to paste into pools.json
# Requires: solana CLI, python3

set -euo pipefail

POOL_ID="${1:?Usage: $0 <pool_pubkey> [rpc_url]}"
RPC="${2:-https://api.mainnet-beta.solana.com}"
TICK_ARRAY_SIZE=88

echo "Fetching Orca Whirlpool: $POOL_ID ..." >&2

python3 - <<PYEOF
import subprocess, json, base64, struct, sys

pool_id = "$POOL_ID"
rpc = "$RPC"
tick_array_size = $TICK_ARRAY_SIZE

# Fetch pool state account
result = subprocess.run(
    ["solana", "account", pool_id, "--output", "json", "--url", rpc],
    capture_output=True, text=True
)
if result.returncode != 0:
    print("Error fetching account:", result.stderr, file=sys.stderr)
    sys.exit(1)

data = base64.b64decode(json.loads(result.stdout)["account"]["data"][0])

# Orca Whirlpool layout (after 8-byte discriminator):
# whirlpools_config: Pubkey    offset 8   (+32)
# token_mint_a:      Pubkey    offset 101 (+32)
# token_vault_a:     Pubkey    offset 133 (+32)
# token_mint_b:      Pubkey    offset 181 (+32)
# token_vault_b:     Pubkey    offset 213 (+32)
# tick_spacing:      u16       offset 41
# fee_rate:          u16       offset 43  (in hundredths of a bip = fee_rate / 10000 bps)
# sqrt_price:        u128      offset 65  (Q64.64)
# tick_current:      i32       offset 81
# oracle:            Pubkey    offset 261 (+32)

def read_pubkey(data, offset):
    return base58_encode(data[offset:offset+32])

def base58_encode(b):
    import hashlib
    alphabet = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
    n = int.from_bytes(b, "big")
    result = b""
    while n > 0:
        n, rem = divmod(n, 58)
        result = bytes([alphabet[rem]]) + result
    pad = len(b) - len(b.lstrip(b"\\x00"))
    return (bytes([alphabet[0]]) * pad + result).decode()

token_mint_a  = read_pubkey(data, 101)
token_vault_a = read_pubkey(data, 133)
token_mint_b  = read_pubkey(data, 181)
token_vault_b = read_pubkey(data, 213)
tick_spacing  = struct.unpack_from("<H", data, 41)[0]
fee_rate_raw  = struct.unpack_from("<H", data, 43)[0]  # hundred-thousandths
fee_bps       = fee_rate_raw // 10  # convert to bps
sqrt_price    = int.from_bytes(data[65:81], "little")
tick_current  = struct.unpack_from("<i", data, 81)[0]
oracle        = read_pubkey(data, 261)

# Derive tick array PDAs
import hashlib

def find_tick_array_start(tick, tick_spacing):
    # tick array start = floor(tick / (tick_spacing * TICK_ARRAY_SIZE)) * (tick_spacing * TICK_ARRAY_SIZE)
    ts = tick_spacing * tick_array_size
    return (tick // ts) * ts

def derive_tick_array_pda(pool_pubkey_b58, start_tick, program_id="whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc"):
    # PDA: seeds = [b"tick_array", pool_pubkey_bytes, start_tick.to_le_bytes(4)]
    import ctypes, os

    def base58_decode(s):
        alphabet = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
        n = 0
        for c in s:
            n = n * 58 + alphabet.index(c)
        result = n.to_bytes(32, "big")
        return result

    pool_bytes = base58_decode(pool_pubkey_b58)
    program_bytes = base58_decode(program_id)
    start_bytes = struct.pack("<i", start_tick)

    # SHA256 PDA derivation (simplified — use solana CLI for production)
    print(f"  # tick_array start={start_tick}: derive PDA manually or use:", file=sys.stderr)
    print(f"  # solana find-program-derived-address {program_id} tick_array {pool_pubkey_b58} bytes:{start_bytes.hex()}", file=sys.stderr)
    return f"DERIVE_PDA_tick_array_start_{start_tick}"

start0 = find_tick_array_start(tick_current, tick_spacing)
start1 = start0 + tick_spacing * tick_array_size
start2 = start0 - tick_spacing * tick_array_size

ta0 = derive_tick_array_pda(pool_id, start0)
ta1 = derive_tick_array_pda(pool_id, start1)
ta2 = derive_tick_array_pda(pool_id, start2)

entry = {
    "id": pool_id,
    "dex": "orca_whirlpool",
    "token_a": token_mint_a,
    "token_b": token_mint_b,
    "vault_a": token_vault_a,
    "vault_b": token_vault_b,
    "fee_bps": fee_bps,
    "state_account": pool_id,
    "extra": {
        "tick_array_0": ta0,
        "tick_array_1": ta1,
        "tick_array_2": ta2,
        "oracle": oracle
    }
}

print(json.dumps(entry, indent=2))
print(f"\n# tick_spacing={tick_spacing}, tick_current={tick_current}", file=sys.stderr)
print(f"# fee_rate_raw={fee_rate_raw} → fee_bps={fee_bps}", file=sys.stderr)
PYEOF
