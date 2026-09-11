#!/usr/bin/env bash
# Fixture drift check — RUN BEFORE EVERY DEPLOY.
#
# Why this exists: the two Metaplex Core CPIs in lib.rs (buyback_core and
# withdraw_core) are hand-encoded — instruction data is the literal
# `[14, 0]` and the account list is seven hand-built AccountMetas, with no
# mpl-core crate dependency. Nothing in the toolchain would notice if Metaplex
# changed TransferV1's discriminator or account order: there is no matching
# type to fail compilation, and Cargo.lock has no mpl-core entry to move.
#
# Worse, the tests cannot notice either. programs/tangem-gacha-vault/tests/
# {pnft,core}.rs load the DUMPED programs from tests/fixtures/, so they keep
# testing the version of Metaplex that was current when the fixture was taken.
# A green suite proves the CPI matches the fixture, not mainnet.
#
# This script closes that gap the only way it can be closed — over the network.
# It re-dumps each upstream program from mainnet-beta and compares it with the
# committed fixture. A mismatch is not automatically a break: Metaplex may have
# redeployed with unrelated changes. It means STOP AND LOOK — refresh the
# fixture, re-run `cargo test -p tangem-gacha-vault --tests`, and re-read the
# TransferV1 account order before shipping.
#
#   ./scripts/check_fixtures.sh
#
# Exit codes: 0 = all fixtures current, 1 = drift, 2 = could not check.

set -uo pipefail

STACK_LOG="${STACK_LOG:-}"

cd "$(dirname "$0")/.." || exit 2
FIXTURES="tests/fixtures"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

command -v solana >/dev/null 2>&1 || { echo "solana CLI not found"; exit 2; }

# program id -> fixture file. mpl-core is the load-bearing one (hand-encoded
# CPI); the other two are used through the mpl-token-metadata crate, so a
# breaking change there WOULD surface at compile time — they are checked
# anyway because a silently stale fixture makes the pNFT tests prove less than
# they claim.
PROGRAMS="
CoREENxT6tW1HoK8ypY1SxRMZTcVPm7R94rH4PZNhX7d mpl_core.so
metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s mpl_token_metadata.so
auth9SigNpDKz4sJJ1DfCTuZrZNSAgh9sFD3rboVmgg mpl_token_auth_rules.so
"

drift=0
checked=0

while read -r id file; do
  [ -z "$id" ] && continue
  local_file="$FIXTURES/$file"
  if [ ! -f "$local_file" ]; then
    echo "MISSING  $file — fixture not in the repo"
    drift=1
    continue
  fi

  if ! solana program dump -u m "$id" "$TMP/$file" >/dev/null 2>&1; then
    echo "UNCHECKED $file — could not dump $id from mainnet-beta"
    exit 2
  fi

  have=$(shasum -a 256 "$local_file" | cut -d' ' -f1)
  want=$(shasum -a 256 "$TMP/$file" | cut -d' ' -f1)
  checked=$((checked + 1))

  if [ "$have" = "$want" ]; then
    echo "OK       $file"
  else
    drift=1
    echo "DRIFT    $file"
    echo "           committed: $have"
    echo "           mainnet:   $want"
    if [ "$file" = "mpl_core.so" ]; then
      echo "           ^ THIS ONE GATES THE DEPLOY. buyback_core and withdraw_core"
      echo "             hand-encode TransferV1 as data=[14,0] with 7 accounts."
      echo "             Re-read mpl-core's TransferV1 before shipping:"
      echo "             asset(w), collection, payer(s,w), authority(s), new_owner,"
      echo "             system_program=None, log_wrapper=None"
      echo "             (absent optionals are passed as the mpl-core program id)."
    fi
    echo "           Refresh: solana program dump -u m $id $local_file"
    echo "           Then:    cargo test -p tangem-gacha-vault --tests"
  fi
done <<< "$PROGRAMS"

# --- SBF stack guard -------------------------------------------------------
# A generated try_accounts frame that exceeds the 4 KB SBF limit is only a
# WARNING: cargo/anchor exit 0 and still write the .so. In that artifact the
# spilled bytes corrupt deserialization — OpenPack overflowing by 8 bytes made
# `config.paused` read true from a zero byte, killing every spin with error
# 6000. So the log must be checked, and NOT checking it must fail too.
#
# This used to default to SKIPPED and then print "Safe to deploy" with exit 0,
# which is the same outcome as passing for anyone reading the exit code.
echo
if [ -z "$STACK_LOG" ]; then
  echo "FAIL     SBF stack check not run — no build log supplied."
  echo "         anchor build 2>&1 | tee build.log && STACK_LOG=build.log $0"
  drift=1
elif [ ! -f "$STACK_LOG" ]; then
  echo "FAIL     SBF stack check not run — '$STACK_LOG' does not exist."
  drift=1
elif grep -q "Stack offset" "$STACK_LOG"; then
  echo "STACK OVERFLOW in $STACK_LOG:"
  grep -n "Stack offset" "$STACK_LOG" | head -5
  echo "  ^ a generated try_accounts frame exceeded the 4 KB SBF limit."
  echo "    cargo/anchor exit 0 on this and still write the .so — do not deploy."
  drift=1
else
  echo "OK       no SBF stack-offset warning in $STACK_LOG"
fi

# cc_buyback.so is NOT a mainnet fixture — it is not deployed anywhere, so there
# is nothing to diff it against. It is hand-copied from
# gachamachine/solana/target/deploy, and a stale copy is the quiet failure mode:
# the suite goes green against the PREVIOUS ABI. Pin its hash so an accidental or
# forgotten swap is visible, and so the pin has to be updated deliberately when
# CC ships a new build.
echo
CC_SO=tests/fixtures/cc_buyback.so
CC_PIN=tests/fixtures/cc_buyback.so.sha256
if [ ! -f "$CC_SO" ]; then
  echo "FAIL     $CC_SO missing — copy it from gachamachine/solana/target/deploy."
  drift=1
elif [ ! -f "$CC_PIN" ]; then
  echo "FAIL     $CC_PIN missing — record it with: shasum -a 256 $CC_SO | awk '{print \$1}' > $CC_PIN"
  drift=1
else
  have=$(shasum -a 256 "$CC_SO" | awk '{print $1}')
  want=$(tr -d '[:space:]' < "$CC_PIN")
  if [ "$have" != "$want" ]; then
    echo "FAIL     cc_buyback.so does not match its pin."
    echo "         pinned $want"
    echo "         actual $have"
    echo "         If CC shipped a new build, update the pin in the same commit."
    drift=1
  else
    echo "OK       cc_buyback.so matches its pin (${have:0:16}...)"
  fi
fi

echo
if [ "$drift" -ne 0 ]; then
  echo "FIXTURE DRIFT — do not deploy until reviewed (see above)."
  exit 1
fi
echo "All $checked fixtures match mainnet-beta, SBF stack clean. Safe to deploy."
