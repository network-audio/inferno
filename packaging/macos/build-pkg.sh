#!/bin/sh
# Builds the macOS installer of the Inferno sound cards (the service, the Core Audio plug-in, the settings app):
#
#   packaging/macos/build-pkg.sh [--version 0.8.0] [--out inferno-soundcard-macos.pkg] [--universal] [--debug]
#
# --universal builds for Apple silicon and Intel (both Rust targets must be installed), otherwise for this Mac.
# Signs with CODESIGN_IDENTITY (a Developer ID Application identity; ad hoc without one) and the package with
# PKG_SIGN_IDENTITY (a Developer ID Installer identity), if set.
set -eu

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
HERE="$ROOT/packaging/macos"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/inferno_soundcard/Cargo.toml" | head -n 1)
OUT="$ROOT/inferno-soundcard-macos.pkg"
UNIVERSAL=0
PROFILE=release
while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    --universal) UNIVERSAL=1; shift ;;
    --debug) PROFILE=debug; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

cd "$ROOT"
PROFILE_FLAG=$([ "$PROFILE" = release ] && echo --release || true)
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
BIN="$STAGE/bin"
mkdir -p "$BIN"
if [ "$UNIVERSAL" = 1 ]; then
  for target in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build --locked $PROFILE_FLAG --target "$target" -p inferno_soundcard -p inferno_coreaudio
  done
  for file in inferno_soundcard libinferno_coreaudio.dylib; do
    lipo -create -output "$BIN/$file" "target/aarch64-apple-darwin/$PROFILE/$file" "target/x86_64-apple-darwin/$PROFILE/$file"
  done
else
  cargo build --locked $PROFILE_FLAG -p inferno_soundcard -p inferno_coreaudio
  cp "target/$PROFILE/inferno_soundcard" "target/$PROFILE/libinferno_coreaudio.dylib" "$BIN/"
fi

ROOTFS="$STAGE/root"
DRIVER="$ROOTFS/Library/Audio/Plug-Ins/HAL/Inferno.driver"
SUPPORT="$ROOTFS/Library/Application Support/Inferno"
APP="$ROOTFS/Applications/Inferno Sound Cards.app"
mkdir -p "$DRIVER/Contents/MacOS" "$SUPPORT/bin" "$ROOTFS/Library/LaunchDaemons" "$ROOTFS/private/etc/newsyslog.d" \
  "$APP/Contents/MacOS"
sed "s/@VERSION@/$VERSION/g" "$HERE/Info.plist" > "$DRIVER/Contents/Info.plist"
cp "$BIN/libinferno_coreaudio.dylib" "$DRIVER/Contents/MacOS/Inferno"
cp "$BIN/inferno_soundcard" "$SUPPORT/bin/"
cp "$HERE/uninstall.sh" "$SUPPORT/"
cp "$HERE/org.network-audio.inferno.soundcard.plist" "$ROOTFS/Library/LaunchDaemons/"
cp "$HERE/newsyslog.conf" "$ROOTFS/private/etc/newsyslog.d/inferno-soundcard.conf"
sed "s/@VERSION@/$VERSION/g" "$HERE/settings-app-Info.plist" > "$APP/Contents/Info.plist"
cp "$HERE/open-settings" "$APP/Contents/MacOS/"

# no extended attributes (e.g. provenance) in the payload, as AppleDouble files
xattr -cr "$ROOTFS"
export COPYFILE_DISABLE=1

IDENTITY="${CODESIGN_IDENTITY:--}"
sign() {
  if [ "$IDENTITY" = - ]; then
    codesign --force --sign - "$@"
  else
    codesign --force --sign "$IDENTITY" --options runtime --timestamp "$@"
  fi
}
sign "$SUPPORT/bin/inferno_soundcard"
sign "$DRIVER"
sign "$APP"

# installed where they are, even if a bundle of the same id is elsewhere
pkgbuild --analyze --root "$ROOTFS" "$STAGE/components.plist" >/dev/null
i=0
while /usr/libexec/PlistBuddy -c "Set :$i:BundleIsRelocatable false" "$STAGE/components.plist" 2>/dev/null; do
  i=$((i + 1))
done

set -- --root "$ROOTFS" --component-plist "$STAGE/components.plist" --scripts "$HERE/scripts" \
  --identifier org.network-audio.inferno.soundcard --version "$VERSION" --install-location /
if [ -n "${PKG_SIGN_IDENTITY:-}" ]; then
  set -- "$@" --sign "$PKG_SIGN_IDENTITY"
fi
pkgbuild "$@" "$OUT"
echo "built $OUT ($VERSION)"
