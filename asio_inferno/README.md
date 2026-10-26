# Inferno ASIO driver (Windows)

An ASIO driver making Inferno a Dante virtual soundcard for Windows audio applications (DAWs and other ASIO
hosts). Each application using it is a Dante device of its own, appearing on the network while the application
uses the driver. Route its channels with Dante Controller or [netaudio](https://github.com/chris-ritsen/network-audio-controller).

**Status: experimental.** It's tested in CI with a minimal ASIO host against other Inferno devices and a simulated
PTP leader (see [`test/windows_e2e`](../test/windows_e2e)), not yet with DAWs or Dante hardware.

## Install

Build (x64 only), then register the DLL from an administrator prompt:

    cargo build --release -p asio_inferno
    regsvr32 target\release\inferno_asio.dll

ASIO hosts then list **Inferno**. `regsvr32 /u target\release\inferno_asio.dll` removes it. Allow UDP 319 and 320
(PTP clock) and the device's ports through Windows Firewall when asked.

## Clock

The device follows the Dante network's PTPv1 clock leader (a Dante device) with Inferno's built-in PTPv1
follower: no PTP daemon is needed, but the network needs a leader. Starting the driver waits up to 8 s for the
clock; if it doesn't come, the host shows why (`getErrorMessage`), and the log has details.

The sample rate is the network's (`SAMPLE_RATE`, default 48000): the driver accepts no other.

## Settings

The ASIO control panel button opens `%APPDATA%\Inferno\inferno_asio.conf` (created with commented examples):
`KEY=VALUE` lines with the [settings](../README.md#settings) of other Inferno integrations, e.g. `NAME`,
`RX_CHANNELS`, `TX_CHANNELS`, `BIND_IP`, latencies. `INFERNO_*` environment variables override them;
`INFERNO_ASIO_CONFIG` sets another file. Restart the application after changing them.

Logs go to `%TEMP%\inferno_asio.log` (level: `RUST_LOG`, default `info`).

## How it works

* buffer sizes 32 to 4096 frames, powers of 2 (preferred 256); 32-bit integer samples
* input latency: a period plus the receive latency; output latency: 2 periods plus the transmit latency
  (the host's outputs of a buffer switch are transmitted 2 periods later)
* buffer switches are timed by the network clock, from a thread registered with MMCSS ("Pro Audio")
* stopping and starting (transport, buffer size changes) keeps the device and its subscriptions

The ASIO interface is implemented from its published ABI, without Steinberg's SDK. ASIO is a trademark of
Steinberg Media Technologies GmbH.
