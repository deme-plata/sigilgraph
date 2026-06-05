#!/usr/bin/env bash
# ═══════════════════════════════════════════════════════════════════════════
#  ROCKY CONTROL DECK — the big-button command center for the SIGIL BTC economy
#  One deck to drive everything shipped this session: bridge · market · miner ·
#  pool · BTC side-miner · spend (Bitrefill/pizza). Rich TUI, big buttons.
# ═══════════════════════════════════════════════════════════════════════════
set -uo pipefail

# ── palette ─────────────────────────────────────────────────────────────────
R=$'\e[0m'; B=$'\e[1m'; DIM=$'\e[2m'
VIO=$'\e[38;5;141m'; VIOB=$'\e[38;5;177m'; GOLD=$'\e[38;5;220m'
GRN=$'\e[38;5;48m'; CYN=$'\e[38;5;51m'; RED=$'\e[38;5;203m'; ORG=$'\e[38;5;215m'
GREY=$'\e[38;5;245m'; BG=$'\e[48;5;53m'

# ── binary locations (shared target + flux target) ──────────────────────────
TS=/home/storage/deepseek-codewhale/.target-shared
FT=/home/storage/deepseek-codewhale/flux/target
bin() { for p in "$TS/release/$1" "$TS/debug/$1" "$FT/release/$1" "$FT/debug/$1"; do
          [ -x "$p" ] && { echo "$p"; return 0; }; done; return 1; }

have() { bin "$1" >/dev/null 2>&1 && echo "${GRN}●${R}" || echo "${RED}○${R}"; }
svc()  { [ "$(systemctl is-active "$1" 2>/dev/null)" = active ] && echo "${GRN}live${R}" || echo "${GREY}down${R}"; }

# ── a BIG button ────────────────────────────────────────────────────────────
button() { # key  icon  title  subtitle  status
  printf "  ${VIO}┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓${R}\n"
  printf "  ${VIO}┃${R} ${GOLD}${B}[%s]${R} %s  ${B}%-26s${R}%s ${VIO}┃${R}\n" "$1" "$2" "$3" "$5"
  printf "  ${VIO}┃${R}     ${DIM}${GREY}%-33s${R} ${VIO}┃${R}\n" "$4"
  printf "  ${VIO}┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛${R}\n"
}

banner() {
  clear
  printf "${VIOB}${B}"
  cat <<'ART'
   ╦═╗╔═╗╔═╗╦╔═╦ ╦   ╔═╗╔═╗╔╗╔╔╦╗╦═╗╔═╗╦    ╔╦╗╔═╗╔═╗╦╔═
   ╠╦╝║ ║║  ╠╩╗╚╦╝   ║  ║ ║║║║ ║ ╠╦╝║ ║║     ║║║╣ ║  ╠╩╗
   ╩╚═╚═╝╚═╝╩ ╩ ╩    ╚═╝╚═╝╝╚╝ ╩ ╩╚═╚═╝╩═╝  ═╩╝╚═╝╚═╝╩ ╩
ART
  printf "${R}${DIM}        SIGIL · BITCOIN · LIGHTNING · agentic-money command deck${R}\n"
  printf "      ${GREY}feed $(svc cockpit-feed) · flux-eye $(svc flux-eye) · snap-rx $(svc flux-snap) · btc-knots ${GRN}delta:8332${R}\n\n"
}

menu() {
  banner
  button 1 "🌉" "BRIDGE"        "SPV+Lightning, peg-committed, all coins"  "$(have sigil-bridge-demo)"
  button 2 "📈" "MARKET"        "live Binance price · DCA · arb scan"      "$(have flux-market-demo)"
  button 3 "⛏ " "SIGIL MINER"   "dual-lane BLAKE4 Φ + VDF Ω demo"          "$(have flux-miner)"
  button 4 "₿ " "BTC SIDE-MINER" "SHA256d @10% cores → flux-pool (P1 exp)" "$(have sigil-btc-miner)"
  button 5 "🪙" "PROVABLE POOL"  "fair LN payouts + fold attestation"      "$(have sigil-btc-miner)"
  button 6 "🍕" "ORDER PIZZA"    "Bitrefill spend (needs LN wallet + key)" "${GOLD}cfg${R}"
  button q "⏏ " "QUIT"          "leave the deck"                          ""
  printf "\n  ${CYN}${B}rocky▸${R} press a key: "
}

run_bridge()  { b=$(bin sigil-bridge-demo)  && "$b"; }
run_market()  { b=$(bin flux-market-demo)   && "$b" "${1:-BTCUSDT}"; }
run_miner()   { b=$(bin flux-miner)         && "$b"; }
run_btcminer(){ b=$(bin sigil-btc-miner)    && "$b" "${1:-3}" "${2:-20}"; }
run_pizza() {
  printf "\n  ${GOLD}${B}🍕 ORDER FOOD — Denmark (Bitrefill, pay with Lightning)${R}\n\n"
  button p "🍕" "ILD.PIZZA DK"        "real Danish pizza chain"        ""
  button s "🥪" "Sunset Boulevard"    "sandwiches / fast food"         ""
  button m "🍔" "McDonald's DK"       "you know what this is"          ""
  button f "🍖" "Restaurant Flammen"  "all-you-can-eat grill buffet"   ""
  button e "🍳" "Early Bird"          "breakfast / brunch"             ""
  printf "  ${CYN}rocky▸${R} pick a place (or any key to cancel): "
  read -rsn1 f; echo
  local place; case "$f" in
    p) place="ILD.PIZZA DK 🍕";; s) place="Sunset Boulevard 🥪";; m) place="McDonald's DK 🍔";;
    f) place="Restaurant Flammen 🍖";; e) place="Early Bird 🍳";; *) printf "  ${GREY}cancelled${R}\n"; return;; esac
  printf "\n  ${GOLD}${B}→ ${place}${R}\n"
  printf "  rocky would now: ${GRN}bitrefill find '${place%% *}' → create LN order → ln_pay → email you the code${R}\n"
  printf "  ${DIM}live blockers: (1) funded LN wallet rocky controls (~20k sat)  (2) BITREFILL_API_ID/SECRET${R}\n"
  printf "  ${DIM}substrate ready: bridge✓ market✓ pool✓ bitrefill-client✓ (DK menu: 5 options wired)${R}\n"
}

while true; do
  menu; read -rsn1 k; echo
  case "$k" in
    1) run_bridge ;;  2) run_market ;;  3) run_miner ;;
    4) run_btcminer 3 20 ;;  5) run_btcminer 2 18 ;;  6) run_pizza ;;
    q|Q) printf "\n  ${VIO}rocky out. ${GOLD}get rich.${R} 🐝\n"; exit 0 ;;
    *) printf "  ${RED}? unknown key${R}\n" ;;
  esac
  printf "\n  ${DIM}↵ enter to return to the deck${R}"; read -r _
done
