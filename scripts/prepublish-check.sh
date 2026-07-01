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
#  (c) whole-repo vendor-name grep, EXCLUDING audited-benign mentions (verified
#      2026-07-01) so any NEW/unexpected mention still fails:
#        scripts/prepublish-check.sh / PUBLISH-BLOCKERS.md — name the tokens by design
#        engine/tests/preflop_chart_contract.rs — the guard test ASSERTS absence
#        engine/data/README.md — documents the clean-room correction
#        engine/src/player.rs — cites the 9-max seat-label naming convention
#                               (Lojack etc.), a factual name, NOT range data
data_hit=$(grep -rIliE "$prov_tokens" engine/data --include="*.json" --include="*.csv" --include="*.txt" 2>/dev/null || true)
if python3 -c "import json,sys; s=json.load(open('engine/data/preflop_v2.json')).get('_source','').lower(); sys.exit(0 if ('clean-room' in s and not any(t in s for t in ['gto wizard','gtowizard','pokercoaching','piosolver'])) else 1)" 2>/dev/null; then
  clean_room_ok=1
else
  clean_room_ok=0
fi
allow='scripts/prepublish-check\.sh|PUBLISH-BLOCKERS\.md|engine/tests/preflop_chart_contract\.rs|engine/data/README\.md|engine/src/player\.rs'
src_hit=$(grep -rIliE "$prov_tokens" . "${excludes[@]}" 2>/dev/null | grep -vE "$allow" || true)
if [ -n "$data_hit" ] || [ -n "$src_hit" ] || [ "$clean_room_ok" -ne 1 ]; then
  echo "  BLOCKER: third-party data-provenance issue:"
  [ -n "$data_hit" ] && echo "$data_hit" | sed 's/^/    - vendor token in DATA file: /'
  [ -n "$src_hit" ]  && echo "$src_hit"  | sed 's/^/    - unaudited vendor mention: /'
  [ "$clean_room_ok" -ne 1 ] && echo "    - preflop_v2.json _source is NOT clean-room"
  fail=1
else
  echo "  ok (engine/data clean · _source=clean-room · only audited vendor mentions remain)"
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
  echo "PASS — no provenance/secret blockers. (Still confirm credential rotation + counsel sign-off per PUBLISH-BLOCKERS.md before going public.)"
else
  echo "FAIL — do NOT publish. Resolve the items above and re-run. See PUBLISH-BLOCKERS.md."
  exit 1
fi
