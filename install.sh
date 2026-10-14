#!/bin/sh
# Installs the Inferno ALSA virtual soundcard: builds the plugin, installs it where alsa-lib looks for
# plugins, defines the "inferno" ALSA device and installs inferno-doctor (checks the setup).
# Optionally sets up PipeWire nodes and the Statime clock.
#
#   ./install.sh                     plugin and ALSA device
#   ./install.sh --pipewire          also PipeWire sink and source for the current user
#   ./install.sh --statime IFACE     also the Statime clock service on network interface IFACE
#   ./install.sh --uninstall         remove everything installed by this script
#   --prebuilt FILE                  install this plugin .so instead of building it (e.g. for packaging)
#   --dry-run                        only print what would be done
#
# System files are installed with sudo when not running as root.
set -eu

cd "$(dirname "$0")"

PIPEWIRE=0
STATIME_IFACE=""
UNINSTALL=0
DRY_RUN=0
PREBUILT=""
while [ $# -gt 0 ]; do
    case "$1" in
        --pipewire) PIPEWIRE=1 ;;
        --statime) shift; STATIME_IFACE="${1:?--statime needs a network interface name}" ;;
        --uninstall) UNINSTALL=1 ;;
        --prebuilt) shift; PREBUILT="${1:?--prebuilt needs the plugin file}" ;;
        --dry-run) DRY_RUN=1 ;;
        -h|--help) sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $1 (see --help)" >&2; exit 2 ;;
    esac
    shift
done

PLUGIN=libasound_module_pcm_inferno.so
ALSA_CONF=/etc/alsa/conf.d/50-inferno.conf
DOCTOR=/usr/local/bin/inferno-doctor
PW_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/pipewire/pipewire.conf.d"
PW_OVERRIDE_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/pipewire.service.d"
STATIME_CONF=/etc/statime/inferno.toml
STATIME_UNIT=/etc/systemd/system/inferno-statime.service

say() { echo "==> $*"; }
fail() { echo "error: $*" >&2; exit 1; }
run() {
    if [ "$DRY_RUN" = 1 ]; then echo "    $*"; else "$@"; fi
}
as_root() {
    if [ "$(id -u)" = 0 ]; then run "$@"; else run sudo "$@"; fi
}

# The directory alsa-lib loads plugins from: <libdir>/alsa-lib, for the libdir libasound is installed in.
alsa_plugin_dir() {
    if command -v pkg-config >/dev/null 2>&1 && libdir=$(pkg-config --variable=libdir alsa 2>/dev/null) \
        && [ -n "$libdir" ]; then
        echo "$libdir/alsa-lib"
        return
    fi
    for dir in /usr/lib64 /usr/lib/x86_64-linux-gnu /usr/lib/aarch64-linux-gnu /usr/lib/arm-linux-gnueabihf \
        /usr/lib/i386-linux-gnu /usr/lib; do
        if [ -e "$dir/libasound.so.2" ]; then
            echo "$dir/alsa-lib"
            return
        fi
    done
    fail "can't find libasound.so.2: is alsa-lib installed?"
}

PLUGIN_DIR=$(alsa_plugin_dir)

if [ "$UNINSTALL" = 1 ]; then
    say "removing the ALSA plugin and device"
    as_root rm -f "$PLUGIN_DIR/$PLUGIN" "$ALSA_CONF" "$DOCTOR"
    run rm -f "$PW_DIR/50-inferno.conf" "$PW_OVERRIDE_DIR/inferno-clock.conf"
    if [ -e "$STATIME_UNIT" ]; then
        say "removing the Statime service"
        as_root systemctl disable --now inferno-statime.service || true
        as_root rm -f "$STATIME_UNIT" "$STATIME_CONF"
        as_root systemctl daemon-reload
    fi
    say "done. Saved device state is kept in ${XDG_STATE_HOME:-$HOME/.local/state}/inferno_aoip"
    exit 0
fi

if [ -n "$STATIME_IFACE" ]; then
    # check everything first, so that nothing is half-installed
    [ -e "/sys/class/net/$STATIME_IFACE" ] || fail "no network interface $STATIME_IFACE (see: ip link)"
    STATIME_BIN=$(command -v statime) \
        || fail "statime not found in PATH: build and install teodly's fork first, see the README"
fi

if [ -n "$PREBUILT" ]; then
    [ -f "$PREBUILT" ] || fail "no plugin file $PREBUILT"
    BUILT="$PREBUILT"
else
    say "building the ALSA plugin and inferno-doctor"
    command -v cargo >/dev/null 2>&1 || fail "cargo not found: install Rust from https://rustup.rs"
    run cargo build --release -p alsa_pcm_inferno -p inferno_doctor
    BUILT="target/release/$PLUGIN"
fi

say "installing the plugin into $PLUGIN_DIR"
as_root install -D -m 644 "$BUILT" "$PLUGIN_DIR/$PLUGIN"
if [ -z "$PREBUILT" ]; then
    as_root install -D -m 755 target/release/inferno-doctor "$DOCTOR"
fi

if [ -e "$ALSA_CONF" ] && ! cmp -s alsa_pcm_inferno/asoundrc "$ALSA_CONF"; then
    say "keeping your modified $ALSA_CONF (the new default is alsa_pcm_inferno/asoundrc)"
else
    say "defining the ALSA device \"inferno\" in $ALSA_CONF"
    as_root install -D -m 644 alsa_pcm_inferno/asoundrc "$ALSA_CONF"
fi

if [ "$PIPEWIRE" = 1 ]; then
    say "adding PipeWire nodes in $PW_DIR"
    run install -D -m 644 os_integration/pipewire/50-inferno.conf "$PW_DIR/50-inferno.conf"
    # PipeWire's systemd unit blocks the clock syscalls the plugin needs
    run install -D -m 644 os_integration/pipewire/systemd_allow_clock.conf "$PW_OVERRIDE_DIR/inferno-clock.conf"
    say "restart PipeWire to load them: systemctl --user daemon-reload && systemctl --user restart pipewire"
fi

if [ -n "$STATIME_IFACE" ]; then
    say "installing the Statime clock service on $STATIME_IFACE"
    tmp=$(mktemp -d)
    sed "s/@INTERFACE@/$STATIME_IFACE/" os_integration/statime/inferno.toml >"$tmp/inferno.toml"
    sed "s#@STATIME@#$STATIME_BIN#" os_integration/statime/inferno-statime.service >"$tmp/inferno-statime.service"
    as_root install -D -m 644 "$tmp/inferno.toml" "$STATIME_CONF"
    as_root install -D -m 644 "$tmp/inferno-statime.service" "$STATIME_UNIT"
    rm -r "$tmp"
    as_root systemctl daemon-reload
    as_root systemctl enable --now inferno-statime.service
fi

if [ -z "$PREBUILT" ]; then
    say "done. Check the setup with: inferno-doctor"
else
    say "done"
fi
