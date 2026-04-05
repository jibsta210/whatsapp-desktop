#!/bin/bash
# build-macos.sh — Build WhatsApp Desktop for macOS
# Run this on a Mac with Homebrew installed.
#
# Usage:
#   git clone <this-repo>
#   cd whatsapp-desktop
#   chmod +x build-macos.sh
#   ./build-macos.sh
#
# Output: WhatsApp.app bundle in ./dist/

set -euo pipefail

APP_NAME="WhatsApp"
BUNDLE_ID="com.whatsapp.desktop"
VERSION="0.1.0"
DIST_DIR="dist"
APP_DIR="$DIST_DIR/$APP_NAME.app"

echo "=== WhatsApp Desktop — macOS Build ==="

# ── Step 1: Install dependencies via Homebrew ──
echo "[1/5] Checking dependencies..."
if ! command -v brew &>/dev/null; then
    echo "ERROR: Homebrew not found. Install from https://brew.sh"
    exit 1
fi

for pkg in gtk4 libadwaita pkg-config; do
    if ! brew list "$pkg" &>/dev/null; then
        echo "  Installing $pkg..."
        brew install "$pkg"
    else
        echo "  ✓ $pkg"
    fi
done

if ! command -v cargo &>/dev/null; then
    echo "ERROR: Rust not found. Install from https://rustup.rs"
    exit 1
fi
echo "  ✓ cargo $(cargo --version | cut -d' ' -f2)"

# ── Step 2: Build release binary ──
echo "[2/5] Building release binary..."
export PKG_CONFIG_PATH="$(brew --prefix)/lib/pkgconfig:$(brew --prefix)/share/pkgconfig:${PKG_CONFIG_PATH:-}"
cargo build -p whatsapp-desktop --release

BINARY="target/release/whatsapp-desktop"
if [ ! -f "$BINARY" ]; then
    echo "ERROR: Build failed — binary not found"
    exit 1
fi
echo "  ✓ Built $(du -h "$BINARY" | cut -f1) binary"

# ── Step 3: Create .app bundle ──
echo "[3/5] Creating $APP_NAME.app bundle..."
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS"
mkdir -p "$APP_DIR/Contents/Resources"

# Copy binary
cp "$BINARY" "$APP_DIR/Contents/MacOS/whatsapp-desktop"

# Create Info.plist
cat > "$APP_DIR/Contents/Info.plist" << PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>$APP_NAME</string>
    <key>CFBundleDisplayName</key>
    <string>$APP_NAME Desktop</string>
    <key>CFBundleIdentifier</key>
    <string>$BUNDLE_ID</string>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleExecutable</key>
    <string>whatsapp-desktop</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSMinimumSystemVersion</key>
    <string>13.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.social-networking</string>
    <key>NSMicrophoneUsageDescription</key>
    <string>WhatsApp needs microphone access for voice notes.</string>
</dict>
</plist>
PLIST

# Create a launcher script that sets up GTK environment
mv "$APP_DIR/Contents/MacOS/whatsapp-desktop" "$APP_DIR/Contents/MacOS/whatsapp-desktop-bin"
cat > "$APP_DIR/Contents/MacOS/whatsapp-desktop" << 'LAUNCHER'
#!/bin/bash
# Set up GTK4 environment from Homebrew
export HOMEBREW_PREFIX="${HOMEBREW_PREFIX:-$(brew --prefix 2>/dev/null || echo /opt/homebrew)}"
export DYLD_LIBRARY_PATH="$HOMEBREW_PREFIX/lib:${DYLD_LIBRARY_PATH:-}"
export GDK_PIXBUF_MODULE_FILE="$HOMEBREW_PREFIX/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache"
export GI_TYPELIB_PATH="$HOMEBREW_PREFIX/lib/girepository-1.0"
export GSETTINGS_SCHEMA_DIR="$HOMEBREW_PREFIX/share/glib-2.0/schemas"
export GTK_PATH="$HOMEBREW_PREFIX/lib/gtk-4.0"
export XDG_DATA_DIRS="$HOMEBREW_PREFIX/share:${XDG_DATA_DIRS:-/usr/share}"

# Adwaita theme
export ADW_DEBUG_COLOR_SCHEME=prefer-dark

DIR="$(cd "$(dirname "$0")" && pwd)"
exec "$DIR/whatsapp-desktop-bin" "$@"
LAUNCHER
chmod +x "$APP_DIR/Contents/MacOS/whatsapp-desktop"

echo "  ✓ $APP_NAME.app created"

# ── Step 4: Create icon (use system icon if no custom one) ──
echo "[4/5] Setting up icon..."
# Generate a simple green circle icon using sips if no icon file exists
ICON_SRC="desktop/resources/icon.png"
if [ -f "$ICON_SRC" ]; then
    # Convert PNG to icns
    ICONSET_DIR="$DIST_DIR/AppIcon.iconset"
    mkdir -p "$ICONSET_DIR"
    for size in 16 32 64 128 256 512; do
        sips -z $size $size "$ICON_SRC" --out "$ICONSET_DIR/icon_${size}x${size}.png" &>/dev/null
        double=$((size * 2))
        sips -z $double $double "$ICON_SRC" --out "$ICONSET_DIR/icon_${size}x${size}@2x.png" &>/dev/null
    done
    iconutil -c icns -o "$APP_DIR/Contents/Resources/AppIcon.icns" "$ICONSET_DIR" 2>/dev/null || true
    rm -rf "$ICONSET_DIR"
    echo "  ✓ Custom icon set"
else
    echo "  ⚠ No icon.png found at $ICON_SRC — app will use default icon"
fi

# ── Step 5: Summary ──
echo "[5/5] Done!"
echo ""
echo "  App:      $APP_DIR"
echo "  Size:     $(du -sh "$APP_DIR" | cut -f1)"
echo ""
echo "  To install:"
echo "    cp -r $APP_DIR /Applications/"
echo ""
echo "  To run:"
echo "    open /Applications/$APP_NAME.app"
echo ""
echo "  Prerequisites on the Mac:"
echo "    brew install gtk4 libadwaita"
echo ""
echo "  To add to Login Items (auto-start):"
echo "    osascript -e 'tell application \"System Events\" to make login item at end with properties {path:\"/Applications/$APP_NAME.app\", hidden:false}'"
