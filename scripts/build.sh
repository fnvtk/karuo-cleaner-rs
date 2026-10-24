#!/bin/zsh
set -euo pipefail
PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
APP_PATH="$PROJECT_DIR/dist/清空AI.app"
export MACOSX_DEPLOYMENT_TARGET=13.0
cargo build --offline --release --manifest-path "$PROJECT_DIR/Cargo.toml"
mkdir -p "$APP_PATH/Contents/MacOS" "$APP_PATH/Contents/Resources"
cp "$PROJECT_DIR/target/release/karuo-cleaner" "$APP_PATH/Contents/MacOS/KaruoCleaner"
cat > "$APP_PATH/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleName</key><string>清空AI</string>
<key>CFBundleDisplayName</key><string>清空AI</string>
<key>CFBundleIdentifier</key><string>com.karuo.cleaner</string>
<key>CFBundleExecutable</key><string>KaruoCleaner</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>1.4.13</string>
<key>CFBundleVersion</key><string>25</string>
<key>LSMinimumSystemVersion</key><string>13.0</string>
<key>NSHighResolutionCapable</key><true/>
<key>CFBundleIconFile</key><string>AppIcon</string>
</dict></plist>
PLIST
if [[ -f "$PROJECT_DIR/Assets/AppIcon.icns" ]]; then
 cp "$PROJECT_DIR/Assets/AppIcon.icns" "$APP_PATH/Contents/Resources/AppIcon.icns"
fi
/usr/bin/codesign --force --deep --sign - "$APP_PATH"
print "BUILT: $APP_PATH"
