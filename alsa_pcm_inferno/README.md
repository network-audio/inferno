# alsa_pcm_inferno

[Inferno-AoIP](../README.md) ALSA PCM module (virtual soundcard)

# How to use

1. Follow common instructions in [Quick start](../README.md#quick-start) to build `alsa_pcm_inferno` and start clock synchronization daemon.
2. Copy (or symlink) `libasound_module_pcm_inferno.so` from `target/debug/` or `target/release/` directory to `/usr/lib64/alsa-lib/` (Fedora) or `/usr/lib/x86_64-linux-gnu/alsa-lib/` (Debian, Ubuntu). If using CPU architecture other than x86_64, the target directory will be different. `find /usr/lib* -type d -name alsa-lib` if unsure.
3. Add `pcm.` device with type `inferno` to your `.asoundrc` (example [`asoundrc`](asoundrc))
4. In the application there should be a place where usually device name is entered, e.g. `hw:1`. Enter the name of the device created in your `.asoundrc` there. If you've copied the example `asoundrc`, the name is just `inferno`.

Making Dante<->analog converter, or Dante<->Dante bridge, or Dante<->AES67 bridge, at a fraction of the price of an off-the-shelf device, using an SBC and PipeWire, is left as an exercise for the reader.

Application must support 32-bit signed integer audio samples. (Audacity doesn't, but generally using Audacity directly with this plugin is not a good idea) `plug` plugin shipped with ALSA should work as automatic converter for apps that don't support that format (not tested yet).

To change the number of channels, specify the parameters `RX_CHANNELS` and `TX_CHANNELS` of the ALSA `pcm` device.

If you don't want to change anything outside your `$HOME`, you can set ALSA environment variables to make libasound search for configuration file and modules in custom directories. Example is in `test_effect_processor.sh`. This will replace the whole system-wide ALSA configuration with Inferno-only setup for apps that have these environment variables changed.

## Recommended: audio server

This plugin is entirely user-space and contained in a library. It means that the Dante device is emulated only while the ALSA device is open in an application and certainly can't outlive the process.

Stopping and restarting a stream (transport stop/start, xrun recovery: `snd_pcm_drop`/`snd_pcm_prepare`/`snd_pcm_start`) keeps the Dante device, its flows and subscriptions running: captured audio resumes within about the receive latency, transmitted audio as soon as the application has refilled its buffer. While the stream is stopped, silence is transmitted. Only closing the device (or changing its buffer configuration) tears it down: next time it is opened, audio flows have to be established again, which takes a few seconds during which silence is played or recorded.

So the Inferno ALSA PCM is best kept open. The easiest way of ensuring this is using an audio server, for example [JACK](https://jackaudio.org/) or [PipeWire](https://www.pipewire.org/) (see script [`start_pipewire_sink`](start_pipewire_sink)), making sure that automatic suspending of audio device is disabled (or, to save energy, set to a timeout long enough that it won't be annoying). DAWs (e.g. [Ardour](https://ardour.org/), see [below](#daws), [BespokeSynth](https://www.bespokesynth.com/), NOT Audacity) also keep the audio interface open while running.

## Buffer sizes

ALSA has the following buffering settings:

* "buffer size" = whole (ring)buffer size - **does not influence latency**
* "period size" - length of buffer part read/written at once by the application - it's the one that we usually call "buffer", or "latency", because latency does depend on it.
* "periods" = whole_buffer_size / period_size - number of periods per whole buffer.

Unlike ASIO, ALSA allows more than 2 periods per buffer (it is useful for energy saving), and this causes the confusion between "buffer size", "period size" and latency, as we can have large whole buffer **and** low latency, thanks to low period size.

In PipeWire, there's additional setting, similar to ALSA period size: [`api.alsa.headroom`](https://docs.pipewire.org/page_man_pipewire-props_7.html) (in samples), which is *[t]he amount of extra space to keep in the ringbuffer*. Increasing it directly increases the latency.

For receiving, the whole buffer size must be at least `receive latency + ALSA period size` (for PipeWire: `maximum receive latency + api.alsa.period-size + api.alsa.headroom`): received audio is written up to the receive latency ahead of what the application reads. Capture buffers smaller than `RX_LATENCY_NS` + period size are refused (the application gets an error when setting hardware parameters, and Inferno logs `capture buffer of ... frames is too small for RX latency ...`). Use a larger buffer or more periods, or decrease `RX_LATENCY_NS`. Note that:

* maximum receive latency does not necessarily equal `RX_LATENCY_NS` - some transmitting devices with high latency setting may force our receive latency to be higher.
* latency in Dante UI (DC/DVS) is expressed in milliseconds or microseconds, in Inferno (and Dante protocol) - in nanoseconds, in ALSA and PipeWire config - in samples.

When using Inferno in PipeWire, it is a good idea to set `api.alsa.headroom` to a value greater than 0. Actually, it is more important than `api.alsa.period-size`. In theory, as long as the CPU keeps up, `api.alsa.period-size` does not influence stability, but `api.alsa.headroom` does - it is the ***sample clock headroom*** parameter we need because Inferno is multi-threaded.

If a capturing application reads so late that received audio overwrites samples it hasn't read yet (e.g. it allows nearly full buffers), Inferno reports an xrun instead of returning the overwritten audio. Leave the application some slack: with a buffer just big enough (e.g. 512×2 at 10 ms receive latency), small scheduling delays already cause xruns.

`snd_pcm_delay` includes the network latency (receive latency for capture, transmit latency for playback), so applications that use it can compensate for it.

## DAWs

Tested with [Ardour](https://ardour.org/) 8.12 (ALSA backend; tests ran headless): recording and playback of 24-bit audio are bit-exact (with `TX_SOURCE_BIT_DEPTH` = 24, apart from Ardour's declick fades), transport stop/start doesn't restart the device, and xruns recover in place without losing Dante subscriptions.

* Ardour's ALSA backend lists only hardware cards. Start Ardour with `ARDOUR_ALSA_DEVICE=inferno` (the name of the `pcm` device in your `.asoundrc`).
* Choose a buffer big enough for the receive latency (see [Buffer sizes](#buffer-sizes)): periods × period size must be at least `RX_LATENCY_NS` (in samples) + period size; for xrun-free operation, aim for `RX_LATENCY_NS` + 2 × period size. The number of periods is rounded up to a power of two (Ardour's 3 periods run as 4). Settings that worked at 48 kHz:
  * `RX_LATENCY_NS` = 40 ms: 1024 samples × 3 periods, 2048 × 3. 2048 × 2 starts but has too little slack (occasional xruns). Ardour's default 1024 × 2 is refused: Ardour shows `AlsaAudioBackend: failed to allocate parameters` / `Cannot start Audio/MIDI engine`, and Inferno's explanation (`capture buffer of ... frames is too small ...`) is printed to Ardour's standard error, i.e. visible when Ardour is started from a terminal.
  * `RX_LATENCY_NS` = 10 ms: 256 × 2, 512 × 3, 1024 × 2. 512 × 2 starts but has too little slack.
* Ardour doesn't use `snd_pcm_delay`, so enter the network latency as systemic latency in Audio/MIDI Setup: input = receive latency in samples minus 24 (e.g. 1896 at 40 ms, 456 at 10 ms), output = receive latency of the device(s) receiving Ardour's output. These values are derived from the measured network delay and Ardour's own port latencies; Ardour's resulting compensation wasn't verified.
* For bit-exact output set `TX_SOURCE_BIT_DEPTH` to 24 (`TX_SOURCE_BIT_DEPTH "24"` in the `pcm` block of your `.asoundrc`, or `INFERNO_TX_SOURCE_BIT_DEPTH=24` in the environment): by default (32) Inferno dithers when transmitting 24-bit audio.


# Quirks

* No matter whether the app does only capture, only playback or both, Inferno will always emulate both capture and playback device (unless number of channels is set to 0). If the app isn't playing audio, Dante devices receiving from Inferno will report broken stream (🚫 in Dante Controller) because, to save CPU, transmitting thread is not run at all then.
* By default, only a single process can run Inferno because it listens on UDP sockets. To overcome this limitation, specify `INFERNO_ALT_PORT` which will be used as a start of the range of ports. Also specify `INFERNO_PROCESS_ID` and `INFERNO_NAME` (see [main README](../README.md#configuration) for details) to avoid non-unique identifiers (Dante Controller may suffer from double vision in such cases).
  * It is needed when using `alsa_in` and `alsa_out` together with JACK, because these are separate processes.
  * On the other hand, it is not needed when adding a single capture and a single playback device to PipeWire graph, because Inferno looks like a regular ALSA soundcard to PipeWire and it is handled within the PipeWire server itself.
  * However, if you want to add different Inferno virtual devices (e.g. belonging to different clock domains, or listening on a different IP address) to the PipeWire graph, specify their configuration using ALSA device configuration


# Tested apps

|          | Tested versions | Status | Remarks |
|---|---|---|---|
| [PipeWire](https://pipewire.org/) | 1.2.7 | ✅ OK  | needs [service patch](../os_integration/systemd_allow_clock.conf) if launched by systemd |
| [spotifyd](https://github.com/Spotifyd/spotifyd) | ... | ✅ OK  | [source](https://gist.github.com/scientress/b7fd79ac761a8574842b96f15696c2b7) |
| 🔒 HQPlayer | ... | ❔ maybe fixed? | [didn't work](https://github.com/teodly/inferno/issues/9), maybe fixed together with JACK? |
| [JACK](https://jackaudio.org/) | 1.9.22 | ✅❔ experimental | part of containerized automated test; xruns recover without losing Dante subscriptions |
| [Ardour](https://ardour.org/) | 8.12 | ✅❔ experimental | see [DAWs](#daws): needs a large enough buffer and manual latency compensation; tested headless, export not tested |
| Audacity | <= 3.5.1        | ✖ WONTFIX | does not support S32 samples so no zero-copy buffers |

🔒 - non-FOSS apps, discouraged by Inferno developers
