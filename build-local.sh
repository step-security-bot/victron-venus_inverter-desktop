#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

APP_NAME="Inverter Desktop"
BUNDLE_DIR="src-tauri/target/release/bundle"

UPDATE_DEPS=false
CLEAN=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --update-deps) UPDATE_DEPS=true; shift ;;
    --clean) CLEAN=true; shift ;;
    *) echo "Unknown option: $1"; exit 1 ;;
  esac
done

if [[ "$CLEAN" == true || "$UPDATE_DEPS" == true ]]; then
  echo "===> Cleaning project..."
  rm -rf node_modules dist src-tauri/target
  echo "  ✓ Cleaned"
fi

echo ""
if [[ "$UPDATE_DEPS" == true ]]; then
  echo "===> Updating dependencies..."
  pnpm update
  echo ""
  echo "===> Regenerating lockfile..."
  pnpm install
  echo ""
  echo "  ⚠  Commit pnpm-lock.yaml before building:"
  echo "      git add pnpm-lock.yaml && git commit -m 'chore: update deps'"
else
  echo "===> Installing dependencies (frozen lockfile)..."
  pnpm install --frozen-lockfile
fi

echo ""
echo "===> Building desktop plugin artifacts..."
PLUGIN_ARTIFACTS=$(python3 scripts/build-local-plugins.py --print-path)

echo ""
echo "===> Building Tauri application..."
# This script installs the macOS app directly; disk images are release artifacts.
pnpm run tauri build --bundles app --verbose

echo ""
echo "===> Installing ${APP_NAME} to /Applications..."
APP_BUNDLE="${BUNDLE_DIR}/macos/${APP_NAME}.app"
if [ -d "$APP_BUNDLE" ]; then
  python3 scripts/macos_app.py "$APP_BUNDLE"
  echo "  ✓ Installed to /Applications/${APP_NAME}.app"
else
  echo "  ✗ Bundle not found at ${APP_BUNDLE}"
  exit 1
fi

BUNDLE_ID="com.alvit.inverter-dashboard"
echo ""
echo "===> Clearing WKWebView NetworkCache..."
rm -rf "$HOME/Library/Caches/${BUNDLE_ID}/WebKit/NetworkCache"
echo "  ✓ $HOME/Library/Caches/${BUNDLE_ID}/WebKit/NetworkCache"

echo ""
echo "===> Installing the matching local plugin artifacts..."
python3 scripts/build-local-plugins.py --install --artifacts "$PLUGIN_ARTIFACTS"

echo ""
echo "========================================"
echo "  Build complete!"
echo "  App:  /Applications/${APP_NAME}.app"
echo "========================================"

date
