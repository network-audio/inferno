# Inferno sound cards for Windows

Windows sound devices on the Dante network, for any Windows application (browsers, video calls, media players, DAWs through WASAPI). Their playback channels are transmitted, their recording channels received. Route them with Dante Controller or [NetAudio](https://netaudio.app/) (open source, [network-audio-controller](https://github.com/chris-ritsen/network-audio-controller)), like any Dante device.

## How it works

Each sound card is a USB Audio 2.0 device, emulated by the `inferno_soundcard` service and served over [USB/IP](https://www.kernel.org/doc/html/latest/usb/usbip_protocol.html) on `127.0.0.1`. The [shared usbip-audio crate](https://github.com/network-audio/usbip-audio) provides the server and the device. The signed [usbip-win2](https://github.com/vadimgrn/usbip-win2) driver attaches it, and Windows' own `usbaudio2.sys` makes it a sound device. No kernel driver of our own, so nothing to sign.

- One Inferno device carries all the cards. Its TX channels are the cards' playback channels, card after card, and its RX channels their recording channels. The settings page shows which Dante channels each card has.
- The USB transfers complete by the Dante media clock, so Windows plays and records at the network's rate, without resampling of its own. The cards appear once the device has a clock: a PTP leader on the network or, on a network without Dante devices, the service itself, after 5 s (`CLOCK_LEADER=off` among its settings to only follow).
- Recording gets what the receiver made readable, and silence where it hasn't (late or missing packets). It never gets older audio from the ring. Playback is transmitted a margin later than Windows plays it (10 ms by default, the setting `playback_margin_ms`), for Windows' audio engine and the service's threads being late; audio later than that goes out as silence.
- The usbip-win2 driver can be used by other applications at the same time, e.g. [Win AirPlay](https://github.com/network-audio/win-airplay). Each runs its own USB/IP server on a free port and detaches only its own devices.

## Install

Download the installer for your architecture from the [releases page](https://github.com/network-audio/inferno/releases), run it and approve the administrator prompt. It installs usbip-win2 0.9.8.1 if it's missing (uninstalling leaves it, for other applications), and starts the service at sign-in, with administrator rights to attach the devices. Restart Windows if prompted.

## Settings

Click the tray icon, or open `http://127.0.0.1:47800/`. Changes apply right away: the cards restart.

- **Sound cards:** a name, playback channels and recording channels for each, up to 128 each at 44.1 or 48 kHz, 64 at 88.2 or 96 kHz.
- **Playback margin** (1 to 100 ms): raise it if the log warns that playback arrived late, at the cost of as much latency.
- **Name** on the Dante network, and further [settings](../README.md#settings) as `KEY=VALUE` lines, e.g. `BIND_IP` for the network interface or `RX_LATENCY_NS`. The channel counts are the cards'. The sound cards run at the network's sample rate (`SAMPLE_RATE`, 48 kHz by default; 44.1 kHz works too): Windows converts other rates for shared-mode applications.

The configuration is `%APPDATA%\Inferno\soundcard.json` (or `INFERNO_SOUNDCARD_CONFIG`), and `INFERNO_*` environment variables complete it. The log is `%LOCALAPPDATA%\Inferno\soundcard.log`.

Only pages from the settings page itself can change the settings. Requests must be addressed to `127.0.0.1` or `localhost`, and changes need a JSON body with a custom header, which other web sites can't send.

## Limits

- Windows' shared-mode audio engine (what most applications use) opens at most 32 channels of a device. Larger cards work with applications that open them in exclusive mode; split them into several cards for the others.
- The cards use [pid.codes](https://pid.codes)' test USB ids (`1209:0001`) until the project has its own.
- Like the ASIO driver: tested in CI only, not yet with real Dante hardware.

## Development

`cargo run -p inferno_soundcard -- --no-tray` runs the service without the tray icon. Without usbip-win2 it serves the cards but doesn't attach them.

- `tests/usbip_loopback.rs`, on any platform: a USB/IP client in place of Windows plays and records through a card, bit-exact both ways, with Inferno devices transmitting to and receiving from it.
- The Windows end-to-end test's `soundcard` scenario (`test/windows_e2e`) installs usbip-win2 and plays and records with the `wasapi_host` example, in exclusive mode, bit-exact.
- The release workflow builds the installers, installs and uninstalls them on x64 and ARM64, and publishes them for `v*` tags.
