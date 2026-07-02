#!/usr/bin/env bash
# Pre-publish gate for the BluffKing open-source subset
# (engine / mental-poker / mp-wasm / gto-solver).
#
# Must exit 0 before the repo may be pushed to a PUBLIC remote. It fails on
# (1) third-party DATA-provenance problems and (2) generic secret/credential
# formats. Patterns are deliberately generic — this public file names no host,
# domain, or path, so the gate itself never discloses infrastructure.
set -uo pipefail
cd "$(dirname "$0")/.."

fail=0
excludes=(--exclude-dir=target --exclude-dir=.git --exclude-dir=pkg --exclude-dir=pkg-web)
prov_tokens='gto[ ]?wizard|gtowizard|pokercoaching|piosolver'

echo "== 1/2  third-party data-provenance gate =="
# The real redistribution risk is the DATA files. Three-part check:
#  (a) engine/data/ must contain NO vendor token (hard blocker if it does);
#  (b) preflop_v2.json _source must positively read "clean-room" and carry no
#      vendor token;
#  (c) whole-repo, LINE-LEVEL vendor-token snapshot. U51: the old check suppressed
#      whole FILES (grep -l against an allow-regex of paths), so a NEW vendor token
#      ADDED to an already-allowlisted file slipped through. Now every matching
#      LINE is hashed into an expected-hit digest of known-benign mentions
#      (verified 2026-07-01); ANY line — even in a benign file — that is not in the
#      snapshot changes the digest and fails. The known-benign lines live in:
#        scripts/prepublish-check.sh            — this gate names the tokens by design
#        engine/tests/preflop_chart_contract.rs — guard test ASSERTS their absence
#        engine/data/README.md                  — documents the clean-room correction
#        engine/src/player.rs                   — 9-max seat-label naming (factual, not data)
#      If a benign line is legitimately reworded/added, review the printed lines
#      and update EXPECTED_PROV_DIGEST below to the newly-printed digest.
data_hit=$(grep -rIliE "$prov_tokens" engine/data --include="*.json" --include="*.csv" --include="*.txt" 2>/dev/null || true)
if python3 -c "import json,sys; s=json.load(open('engine/data/preflop_v2.json')).get('_source','').lower(); sys.exit(0 if ('clean-room' in s and not any(t in s for t in ['gto wizard','gtowizard','pokercoaching','piosolver'])) else 1)" 2>/dev/null; then
  clean_room_ok=1
else
  clean_room_ok=0
fi
# Line-level hits normalized to "path:content" (line numbers dropped so a benign
# edit that only shifts line numbers doesn't churn the digest), sorted for stability.
prov_hits=$(grep -rIniE "$prov_tokens" . "${excludes[@]}" 2>/dev/null | sed -E 's#^\./##; s/^([^:]+):[0-9]+:/\1:/' | LC_ALL=C sort || true)
prov_digest=$(printf '%s' "$prov_hits" | shasum -a 256 | awk '{print $1}')
EXPECTED_PROV_DIGEST="7d451e2c7ba3f9b7c997268e7e8973337ce411c602542b385bc58b093860792e"
if [ -n "$data_hit" ] || [ "$clean_room_ok" -ne 1 ] || [ "$prov_digest" != "$EXPECTED_PROV_DIGEST" ]; then
  echo "  BLOCKER: third-party data-provenance issue:"
  [ -n "$data_hit" ] && echo "$data_hit" | sed 's/^/    - vendor token in DATA file: /'
  [ "$clean_room_ok" -ne 1 ] && echo "    - preflop_v2.json _source is NOT clean-room"
  if [ "$prov_digest" != "$EXPECTED_PROV_DIGEST" ]; then
    echo "    - vendor-token line-set changed (expected $EXPECTED_PROV_DIGEST, got $prov_digest)."
    echo "      Review each matching line below; if ALL are benign, update EXPECTED_PROV_DIGEST:"
    printf '%s\n' "$prov_hits" | sed 's/^/        /'
  fi
  fail=1
else
  echo "  ok (engine/data clean · _source=clean-room · vendor-token line-set matches snapshot)"
fi

echo "== 2/2  generic secret / credential scan =="
# Generic formats only: AWS/GitHub/Slack/Google/OpenAI-style keys, PEM private
# keys, JWTs, and key=value credential assignments. No project-specific strings.
secret_re="AKIA[0-9A-Z]{16}|ghp_[0-9A-Za-z]{30,}|xox[baprs]-[0-9A-Za-z-]{10,}|AIza[0-9A-Za-z_-]{30,}|sk-[A-Za-z0-9-]{20,}|-----BEGIN [A-Z ]*PRIVATE KEY-----|eyJ[A-Za-z0-9_-]{20,}[.][A-Za-z0-9_-]{20,}[.][A-Za-z0-9_-]{10,}|(password|passwd|secret|api[_-]?key|access[_-]?key|token)[[:space:]]*[:=][[:space:]]*[A-Za-z0-9/+=_-]{12,}"
secrets=$(grep -rIlE "$secret_re" . "${excludes[@]}" --exclude=prepublish-check.sh 2>/dev/null || true)
if [ -n "$secrets" ]; then
  echo "  BLOCKER: potential secret / credential string present in:"
  echo "$secrets" | sed 's/^/    - /'
  fail=1
else
  echo "  ok"
fi

echo
if [ "$fail" -eq 0 ]; then
  # U52: this repo is already PUBLIC (see PUBLISH-BLOCKERS.md). This runs as a
  # blocking CI gate on every push/PR — the pre-publication credential-rotation
  # and counsel sign-off were completed before release; this is not a pre-publish
  # checklist.
  echo "PASS — no provenance/secret blockers. (Blocking CI gate for the already-public repo; the pre-publication rotation + counsel sign-off are recorded in PUBLISH-BLOCKERS.md.)"
else
  echo "FAIL — do NOT publish. Resolve the items above and re-run. See PUBLISH-BLOCKERS.md."
  exit 1
fi
