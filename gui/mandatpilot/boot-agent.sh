#!/usr/bin/env bash
# boot-agent.sh — start the MandatPilot onboarding AI as a headless `claude code -p` agent.
#
# The agent's PRIME DIRECTIVE: open the MitID modal for the customer as fast as possible,
# then take them through the $2 onramp. It speaks ONLY consumer language (MitID, verificeret,
# credits, kr, $2) — never coin/wallet/blockchain (invisible-chain rule, sigil-mandat).
#
# Usage:
#   ./boot-agent.sh                 # interactive-ish single shot
#   MANDATD=http://127.0.0.1:8791 ./boot-agent.sh "ny kunde lige landet på siden"
#
# Requires: the MandatPilot MCP wired into Claude Code (mandatd /credits,/topup,/verify,/watch).

set -euo pipefail
MANDATD="${MANDATD:-http://127.0.0.1:8791}"
PROMPT="${1:-En ny kunde har netop åbnet MandatPilot. Kom dem i gang hurtigst muligt.}"

read -r -d '' SYSTEM <<EOF || true
Du er Mandat-AI, onboarding-agenten for MandatPilot (fluxapp.xyz).
PRIORITET #1: Få kunden i gang HURTIGST muligt. Din ALLERFØRSTE handling er at
udløse MitID-modalen i sidepanelet (emit: {"action":"open_mitid_modal"}) — vent
ikke, spørg ikke først. Derefter: guide til $2-onramp (\$2 = 100 credits) via
MandatPilot-MCP'ens topup. Tal KUN forbruger-sprog: MitID, verificeret, credits,
kr, \$2. Sig ALDRIG coin/wallet/blockchain/QUG. Skrive-handlinger kræver friskt
MitID-samtykke. mandatd: ${MANDATD}
EOF

# Headless print-mode agent. --append-system-prompt injects the MitID-first directive.
# The panel (index.html) listens for the {"action":"open_mitid_modal"} the agent emits first.
exec claude code -p "$PROMPT" \
  --append-system-prompt "$SYSTEM" \
  --output-format stream-json
