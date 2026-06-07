#!/bin/sh
# Install flux:// protocol handler for SIGIL local wallet (+ TRON)
set -e
PORT=${FLUX_WALLET_PORT:-8443}
APPS_DIR="${HOME}/.local/share/applications"
mkdir -p "$APPS_DIR"

cat > "$APPS_DIR/sigil-flux-wallet.desktop" << EOF
[Desktop Entry]
Type=Application
Name=SIGIL Flux Wallet
Comment=Local SIGIL + TRON wallet for node operators — press w for wallet
Exec=xdg-open https://localhost:${PORT}/%u
Terminal=true
Categories=Network;
MimeType=x-scheme-handler/flux;
NoDisplay=true
EOF
chmod 755 "$APPS_DIR/sigil-flux-wallet.desktop"
xdg-mime default sigil-flux-wallet.desktop x-scheme-handler/flux 2>/dev/null || true

# CLI handler
FLUX_BIN="${HOME}/.flux/bin"
mkdir -p "$FLUX_BIN"
cat > "$FLUX_BIN/flux-protocol-handler" << 'HANDLER'
#!/bin/sh
URI="$1"
case "$URI" in
  flux://wallet*)     xdg-open "https://localhost:${PORT:-8443}/wallet/" ;;
  flux://sigil-top*)  xdg-open "https://localhost:${PORT:-8443}/sigil-top/" ;;
  flux://explorer*)   xdg-open "https://localhost:${PORT:-8443}/explorer/" ;;
  flux://tron*)       xdg-open "https://localhost:${PORT:-8443}/tron-wallet/" ;;
  flux://bridge*)     xdg-open "https://localhost:${PORT:-8443}/bridge-status" ;;
  flux://*)           xdg-open "https://localhost:${PORT:-8443}/" ;;
esac
HANDLER
chmod 755 "$FLUX_BIN/flux-protocol-handler"

echo ""
echo "✓ flux:// protocol handler installed (with TRON support)"
echo "  Try: xdg-open flux://wallet"
echo "       xdg-open flux://tron"
echo ""
