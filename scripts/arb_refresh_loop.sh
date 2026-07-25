#!/usr/bin/env bash
# arb_refresh_loop.sh — periodic arb book refresh.
#
# One cycle: scan --apply → (on change) extend the ALT → SIGHUP the bot so it reloads.
# Deliberately dumb: no pool logic here, and no process management — the bot re-execs
# itself on SIGHUP (same PID, same terminal).
#
# Usage: ./scripts/arb_refresh_loop.sh            # loop forever
#        ONESHOT=1 ./scripts/arb_refresh_loop.sh  # single cycle (cron / manual)
set -uo pipefail
cd "$(dirname "$0")/.."
set -a; [ -f .env ] && . ./.env; set +a

INTERVAL="${ARB_SCAN_INTERVAL_SECS:-21600}"   # ~6h

one_cycle() {
  echo "[$(date -u +%FT%TZ)] arb refresh: scanning"
  node scripts/scan_arb_pools.js --apply
  local rc=$?
  case "$rc" in
    0)  echo "  book changed — extending ALT"
        if ! cargo run --release --bin solana-mev -- --init-alt; then
          echo "  !! --init-alt FAILED — not sending SIGHUP (book+ALT stay consistent)" >&2
          return 1
        fi
        local pid; pid="$(pgrep -f 'target/release/solana-mev' | head -1)"
        if [ -n "$pid" ]; then echo "  HUP -> $pid"; kill -HUP "$pid";
        else echo "  no bot running — book+ALT ready for next start"; fi ;;
    10) echo "  no change" ;;
    *)  echo "  !! scan FAILED (rc=$rc) — book untouched" >&2; return 1 ;;
  esac
}

if [ -n "${ONESHOT:-}" ]; then one_cycle; exit $?; fi
while true; do one_cycle || true; sleep "$INTERVAL"; done
