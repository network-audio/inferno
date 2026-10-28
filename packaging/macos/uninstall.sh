#!/bin/sh
# Removes the Inferno sound cards: sudo "/Library/Application Support/Inferno/uninstall.sh" [--purge]
# --purge also removes the settings, logs and the service's account.
set -u

if [ "$(id -u)" -ne 0 ]; then
  echo "run with sudo" >&2
  exit 1
fi
LABEL=org.network-audio.inferno.soundcard
DIR="/Library/Application Support/Inferno"

launchctl bootout "system/$LABEL" 2>/dev/null || true
rm -f "/Library/LaunchDaemons/$LABEL.plist" /etc/newsyslog.d/inferno-soundcard.conf
rm -rf /Library/Audio/Plug-Ins/HAL/Inferno.driver "/Applications/Inferno Sound Cards.app" "$DIR/bin"
killall coreaudiod 2>/dev/null || true
pkgutil --forget org.network-audio.inferno.soundcard >/dev/null 2>&1 || true

if [ "${1:-}" = "--purge" ]; then
  rm -rf "$DIR" /Library/Logs/Inferno
  dscl . -delete /Users/_inferno 2>/dev/null || true
  dscl . -delete /Groups/_inferno 2>/dev/null || true
else
  rm -f "$DIR/uninstall.sh"
  echo "kept the settings in $DIR (--purge removes them)"
fi
echo "Inferno sound cards removed"
