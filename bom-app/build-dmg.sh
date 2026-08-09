#!/bin/bash
set -euo pipefail

echo "🔨 Building BOM app DMG for macOS..."
cd "$(dirname "$0")"

# 1. Install dependencies if needed
if ! command -v create-dmg &> /dev/null; then
    echo "📦 Installing create-dmg..."
    brew install create-dmg
fi

# 2. Build frontend
echo "📦 Installing frontend dependencies..."
npm ci

echo "🏗️  Building frontend..."
npm run build

# 3. Build with Tauri
echo "🏗️  Building BOM app with Tauri..."
npm run tauri -- build --target aarch64-apple-darwin --runner cargo

# 4. Code sign the app (ad-hoc)
echo "✍️  Signing app..."
codesign --force --deep --sign - "target/aarch64-apple-darwin/release/bundle/macos/KiCad BOM Tool.app"

# 5. Create DMG
echo "📀 Creating DMG..."
release_dir="target/aarch64-apple-darwin/release"
staging_dir="target/aarch64-apple-darwin/dmg-staging"
rm -rf "$staging_dir"
mkdir -p "$staging_dir"
cp -R "$release_dir/KiCad Auto Importer.app" "$staging_dir/"
cp -R "$release_dir/KiCad BOM Tool.app" "$staging_dir/"

# Generate DMG background (from kicad-auto-importer)
"$release_dir/kicad-auto-importer" --emit-dmg-background dmg-background.png

create-dmg \
  --volname "KiCad Autotools" \
  --background dmg-background.png \
  --window-size 660 400 \
  --icon-size 128 \
  --icon "KiCad Auto Importer.app" 90 170 \
  --icon "KiCad BOM Tool.app" 270 170 \
  --app-drop-link 450 170 \
  --no-internet-enable \
  "kicad-autotools-aarch64-apple-darwin.dmg" \
  "$staging_dir"

echo "✅ DMG created: kicad-autotools-aarch64-apple-darwin.dmg"
