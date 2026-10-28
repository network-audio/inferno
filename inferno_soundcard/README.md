# Inferno sound cards for Windows and macOS

Sound devices on the Dante network, for any application: on Windows browsers, video calls, media players, DAWs through WASAPI; on macOS anything using Core Audio, DAWs included. Their playback channels are transmitted, their recording channels received. Route them with Dante Controller or [NetAudio](https://netaudio.app/) (open source, [network-audio-controller](https://github.com/chris-ritsen/network-audio-controller)), like any Dante device.

One Inferno device carries all the cards: its TX channels are the cards' playback channels, card after card, and its RX channels their recording channels. The settings page shows which Dante channels each card has. The cards appear once the device has a clock: a PTP leader on the network or, on a network without Dante devices, the service itself, after 5 s (`CLOCK_LEADER=off` among its settings to only follow).

## Windows

### How it works

Each sound card is a USB Audio 2.0 device, emulated by the `inferno_soundcard` service and served over [USB/IP](https://www.kernel.org/doc/html/latest/usb/usbip_protocol.html) on `127.0.0.1`. The [shared usbip-audio crate](https://github.com/network-audio/usbip-audio) provides the server and the device. The signed [usbip-win2](https://github.com/vadimgrn/usbip-win2) driver attaches it, and Windows' own `usbaudio2.sys` makes it a sound device. No kernel driver of our own, so nothing to sign.

- The USB transfers complete by the Dante media clock, so Windows plays and records at the network's rate, without resampling of its own.
- Recording gets what the receiver made readable, and silence where it hasn't (late or missing packets). It never gets older audio from the ring. Playback is transmitted a margin later than Windows plays it (10 ms by default, the setting `playback_margin_ms`), for Windows' audio engine and the service's threads being late; audio later than that goes out as silence.
- The usbip-win2 driver can be used by other applications at the same time, e.g. [Win AirPlay](https://github.com/network-audio/win-airplay). Each runs its own USB/IP server on a free port and detaches only its own devices.

### Install

Download the installer for your architecture from the [releases page](https://github.com/network-audio/inferno/releases), run it and approve the administrator prompt. It installs usbip-win2 0.9.8.1 if it's missing (uninstalling leaves it, for other applications), and starts the service at sign-in, with administrator rights to attach the devices. Restart Windows if prompted.

### Settings

Click the tray icon, or open `http://127.0.0.1:47800/`. Changes apply right away: the cards restart.

- **Sound cards:** a name, playback channels and recording channels for each, up to 128 each at 44.1 or 48 kHz, 64 at 88.2 or 96 kHz.
- **Playback margin** (1 to 100 ms): raise it if the log warns that playback arrived late, at the cost of as much latency.
- **Name** on the Dante network, and further [settings](../README.md#settings) as `KEY=VALUE` lines, e.g. `BIND_IP` for the network interface or `RX_LATENCY_NS`. The channel counts are the cards'. The sound cards run at the network's sample rate (`SAMPLE_RATE`, 48 kHz by default; 44.1 kHz works too): Windows converts other rates for shared-mode applications.

The configuration is `%APPDATA%\Inferno\soundcard.json` (or `INFERNO_SOUNDCARD_CONFIG`), and `INFERNO_*` environment variables complete it. The log is `%LOCALAPPDATA%\Inferno\soundcard.log`.

Only pages from the settings page itself can change the settings. Requests must be addressed to `127.0.0.1` or `localhost`, and changes need a JSON body with a custom header, which other web sites can't send.

### Limits

- Windows' shared-mode audio engine (what most applications use) opens at most 32 channels of a device. Larger cards work with applications that open them in exclusive mode; split them into several cards for the others.
- The cards use [pid.codes](https://pid.codes)' test USB ids (`1209:0001`) until the project has its own.

## macOS

### How it works

Each sound card is a Core Audio device of the Inferno HAL plug-in ([`inferno_coreaudio`](../inferno_coreaudio), an AudioServerPlugIn in `/Library/Audio/Plug-Ins/HAL/Inferno.driver`): Apple's user-space driver model, loaded by Core Audio. No kernel extension.

- The plug-in does no networking. The sound card service runs the Inferno device, as on Windows, as a launchd daemon in an account of its own without privileges (`_inferno`). Its rings, addressed by the Dante media clock, are in memory it shares with the plug-in ([`inferno_soundcard_shm`](../inferno_soundcard_shm)), handed over through the service's Mach service (only to Core Audio): the plug-in plays and records there directly, and Inferno's receiver and transmitter use the same memory.
- The plug-in gives Core Audio the time of every frame from the media clock the service publishes there, so Core Audio plays and records at the network's rate, without resampling. All cards are of one clock domain.
- Recording gets what the receiver made readable, silence where it hasn't. Playback is transmitted the playback margin later than Core Audio plays it. The devices report the network's latency (and the margin), for applications' latency compensation.
- Each card has a volume and a mute control for playback and for recording: System Settings' sliders, the volume keys and applications' volume controls work. At full volume (the default) the audio passes bit-exact; below, the gain is the square of the slider's position (to -64 dB, then silence). Core Audio keeps them, per card, across restarts.
- When the service restarts the cards (new settings) or stops, the devices go, and come back when it has them again.

### Install

The releases' package (`inferno-soundcard-macos.pkg`) is signed with a Developer ID and notarized: open it to install. Or build the package (Rust and the Xcode command line tools needed) and install it:

    packaging/macos/build-pkg.sh
    sudo installer -pkg inferno-soundcard-macos.pkg -target /

Without a Developer ID (`CODESIGN_IDENTITY`, `PKG_SIGN_IDENTITY`), the package is signed ad hoc: install it with `installer`, or open it with a right click if macOS refuses. `sudo "/Library/Application Support/Inferno/uninstall.sh"` removes everything but the settings (`--purge` removes them too).

The release workflow signs and notarizes the package with the repository's secrets: `MACOS_CERTIFICATES_P12` (base64 of a `.p12` with the Developer ID Application and Developer ID Installer identities), `MACOS_CERTIFICATES_PASSWORD` (its password), and an App Store Connect API key for the notary service, `NOTARY_KEY_P8` (the `.p8` file), `NOTARY_KEY_ID` and `NOTARY_ISSUER_ID`. Without them it builds the package signed ad hoc, and doesn't publish it.

### Settings

Open **Inferno Sound Cards** from Applications, or `http://127.0.0.1:47800/`: the same page and settings as on Windows. The cards are the whole Mac's: the configuration is `/Library/Application Support/Inferno/soundcard.json`, the log `/Library/Logs/Inferno/soundcard.log` (rotated). The plug-in logs to the system log: `log show --last 10m --predicate 'eventMessage CONTAINS "Inferno"'`.

### Limits

- Up to 128 playback and 128 recording channels per card at any rate (no shared-mode or USB limit), up to 32 cards.

## Development

`cargo run -p inferno_soundcard -- --no-tray` runs the service without the tray icon. Without usbip-win2 it serves the cards but doesn't attach them. On macOS the plug-in only finds the service started by launchd (its Mach service): install the package.

- `tests/usbip_loopback.rs`, on any platform: a USB/IP client in place of Windows plays and records through a card, bit-exact both ways, with Inferno devices transmitting to and receiving from it.
- `tests/coreaudio_loopback.rs`, on any platform: a client in place of Core Audio runs I/O cycles by the card's clock and plays and records through the shared region as the plug-in does, bit-exact both ways.
- The end-to-end tests' `soundcard` scenarios (`test/e2e`): on Windows, usbip-win2 installed, the `wasapi_host` example plays and records in exclusive mode; on macOS, the package installed, the `coreaudio_host` example (of `inferno_coreaudio`) plays and records through Core Audio. Bit-exact.
- The release workflow builds the installers, installs and uninstalls them on x64 and ARM64, and publishes them for `v*` tags.
