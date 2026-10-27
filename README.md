# Inferno - unofficial implementation of Dante protocol

[GitLab](https://gitlab.com/lumifaza/inferno) | [GitHub](https://github.com/teodly/inferno) | [Principal author's website](https://info.lumifaza.org/)

This project is named after [a place](https://en.wikipedia.org/wiki/Inferno_(Dante)) destined for those who create undocumented network protocols.

Highly experimental for now. I don't recommend using it for serious purposes.

However, chances that it'll break already working Dante network are low.

If you know what you're doing and have basic Linux command line experience, it is perfectly usable for non-critical tasks, e.g. listening to music, playing multitracks through mixing console for mixing practice, recording rehearsals.

Big thanks to [Project Pendulum](https://github.com/pendulum-project) (by [Trifecta Tech Foundation](https://trifectatech.org/)) for creating and maintaining [Statime](https://github.com/pendulum-project/statime) and collaboration on features needed for audiovisual networks functionality! Audio transmission would be much more difficult to implement without it.


# Features
* receiving audio from and sending audio to Dante devices and virtual devices
* works with most features of Dante Controller and [network-audio-controller](https://github.com/chris-ritsen/network-audio-controller) (`netaudio` command line tool)

## Comparison with other AoIP virtual soundcards

|   | **Inferno** | [DVS](https://www.getdante.com/products/software-essentials/dante-virtual-soundcard) | [AES67 Linux daemon](https://github.com/bondagit/aes67-linux-daemon) |
|---|---|---|---|
| Maturity | ⏳ Alpha but used in production | ✅ Production-ready | ✅ Probably stable |
| Platforms | Linux  | Mac, Windows  | Linux |
| Supported protocols | Dante | Dante | AES67 |
| Directly supported audio backends | ALSA | CoreAudio, ASIO, WDM | ALSA |
| Works with DAWs | ✅ Yes ([tested with Ardour](alsa_pcm_inferno/README.md#daws)) | ✅ Yes | ✅ Yes |
| Route audio using Dante Controller patchbay | ✅ Yes! | ✅ Yes | 🚫 AES67->Dante only |
| Configurable using Dante Controller | ⏳ A bit (channel names, TX multicasts) | ✅ Yes | 🚫 No |
| Compatible with Dante Domain Manager | 🚫 No | ✅ Yes | 🚫 No (but AES67 integration possible) |
| Supported clock protocols | [PTPv1](https://github.com/teodly/statime/tree/inferno-dev) ☑️, PTPv2 ☑️ | PTPv1 ✅ | PTPv2 ✅ |
| Clock leader | PTPv2 ☑️ via [Statime](https://github.com/pendulum-project/statime) | 🚫 No (but possible in Dante Via) | ☑️ via external daemon |
| Stream audio from/to modern Dante hardware | ✅ Yes | ✅ Yes | ✅ Yes |
| Stream audio from/to DVS, Dante Via & old Dante hardware | ✅ Yes | ✅ Yes | 🚫 No  |
| Stream audio from/to AES67           | 🚫 No  | 🚫 No  | ✅ Yes |
| Minimum latency | as low as your kernel gets | 4ms | ... |
| Sends & receives multicasts | ✅ Yes | ✅ Yes | ✅ Yes |
| OS integration | Entirely user-space | Kernel driver & user-space services | Kernel driver & user-space helper |
| Lightweight recording app | ✅ Yes (Inferno2pipe) | 🚫 No | ☑️ FFmpeg with RTP input does the trick |
| Disk space & RAM usage | 🌱 Low (~12MB RAM) | 🔥 High | 🌱 Low |
| Written in | Rust | C++, Java | C++, C |
| License | 🥰 FOSS, copyleft | 🔒 Closed source | 🥰 FOSS, copyleft |
| [DRM](https://drm.info/what-is-drm.en.html) | 😊 No | 🔒 Activation required, virtual machines banned | 😊 No |
| Price   | Free of charge | 🤑 50-80 USD ... *for a **device driver*** | Free of charge |
| Privacy | 😊 No tracking | 😡 Registration required, telemetry enabled by default | 😊 No tracking |

* ✅ - usable
* 💣 - experimental
* ☑️ - not a part of this software but integration is easily possible
* ⏳ - will be implemented soon (until 2025-06 probably)
* 🚫 - unimplemented and not planned for the near future

## Quirks, read it before using:
* Dante protocol is undocumented. Everything was reverse-engineered or based on other reverse-engineering projects. Some things in implementation were guessed. So while it works with my setup, it may not work with yours.
* Collisions with NTP clock synchronization have been noticed. Our Statime configuration uses `virtual-system-clock-base = "monotonic_raw"`, which should avoid them, but this fix is young (see [Software timestamping](#software-timestamping)). The built-in clock filter is insufficient. [Fixing it properly is complicated.](https://github.com/pendulum-project/statime/issues/389#issuecomment-2214559362) If your network interface supports hardware timestamping, you may be able to workaround it using `/dev/ptp0` as [`CLOCK_PATH`](#configuration) and a [PTP daemon that does not use global system clock](https://gitlab.freedesktop.org/pipewire/pipewire/-/wikis/AES67#setting-up-ptp-time-sync), but you need to enable PTPv2 (AES67) in one of Dante devices then because ptp4l is not compatible with PTPv1.
  * to fix this in Statime, [PHC-only mode](https://github.com/pendulum-project/statime/issues/517) needs to be implemented for hardware timestamps, and [CLOCK_MONOTONIC-based virtual clock with arbitrary timescale](https://github.com/pendulum-project/statime/issues/389) for software timestamps
* Inferno2pipe is clocked by incoming media flows. When nothing is connected, "time will stop" (i.e. recording will pause) until something is connected again - silence won't be generated unless at least one channel is connected.


# Quick start
For the virtual soundcard (`alsa_pcm_inferno`), on Linux:

1. **Prerequisites:** [Rust](https://rustup.rs/), git and the ALSA development files:
   * Debian/Ubuntu/Mint: `sudo apt install libasound2-dev pkg-config`
   * Fedora/CentOS: `sudo dnf install alsa-lib-devel`
   * Arch: `sudo pacman -S alsa-lib`
2. **Clock:** Inferno follows the Dante network's PTP clock through a PTP daemon, also when only capturing. Build our fork of [Statime](https://github.com/pendulum-project/statime) (it speaks PTPv1, which Dante devices use):
   ```
   git clone --recurse-submodules -b inferno-dev https://github.com/teodly/statime
   cd statime && cargo build --release && sudo install target/release/statime /usr/local/bin/
   ```
   See [Clocking options](#clocking-options) for hardware timestamping and PTPv2.
3. **Install:** clone this repository with `--recursive` (some dependencies are submodules) and run the installer in it, naming the network interface connected to the Dante network:
   ```
   git clone --recursive https://gitlab.com/lumifaza/inferno.git
   cd inferno && ./install.sh --statime eth0 --pipewire
   ```
   It installs the ALSA plugin where your alsa-lib looks for plugins, defines the ALSA device `inferno` (`/etc/alsa/conf.d/50-inferno.conf`), installs `inferno-doctor` and, with `--statime IFACE`, starts Statime as the `inferno-statime` service. `--pipewire` adds an Inferno sink and source to PipeWire, restart PipeWire afterwards. Leave out what you don't need; `./install.sh --help` lists the options, `--uninstall` removes everything again.
4. **Check:** `inferno-doctor` checks the plugin, the settings, the clock, the network ports and realtime permissions, and says what to fix. Run it as the user of the audio application (e.g. PipeWire).
5. **Use:** the ALSA device `inferno`, e.g. `aplay -D inferno ...`, `jackd -dalsa -dinferno -p1024 -n4`, or PipeWire's "Inferno (Dante)" sink and source. It appears on the network while an application has it open: route its channels with Dante Controller or [netaudio](https://github.com/chris-ritsen/network-audio-controller). See [`alsa_pcm_inferno`](alsa_pcm_inferno/README.md) for buffer sizes, DAWs and channel counts, and [Configuration](#configuration) for its settings.

For capturing to a file without an audio stack, see [`Inferno2pipe`](inferno2pipe/README.md) (`cargo build --release -p inferno2pipe`).

**Firewall:** allow incoming UDP 4440, 8800, 4455 and 8700 (or `ALT_PORT` to `ALT_PORT+3`), 5353 (mDNS), 4321 (multicast audio), 319 and 320 (PTP), and audio flows from transmitters (their ports are chosen by the operating system, so can't be known beforehand).

**IPv4 and IPv6:** Dante devices use IPv4 only, and so does Inferno (Dante's protocols carry IPv4 addresses). It works on dual-stack hosts and networks: IPv6 is ignored. Without `BIND_IP`, it uses the address of the IPv4 default route or, if there is none (e.g. a Dante network without a gateway, internet over IPv6 only), an IPv4 address of an interface, link-local 169.254.x.x addresses last; with several interfaces to choose from, set `BIND_IP`.

**Security frameworks:** seccomp, SELinux and similar must not block clock-related syscalls. For PipeWire run by systemd, `install.sh --pipewire` installs the needed override ([`os_integration/pipewire/systemd_allow_clock.conf`](os_integration/pipewire/systemd_allow_clock.conf), as `~/.config/systemd/user/pipewire.service.d/inferno-clock.conf`).

**Several devices on one computer** (e.g. PipeWire and another application each opening `inferno`): the first one uses the standard ports. A further one, if `ALT_PORT` isn't set, picks free ports (from 20000-20999) and, if `PROCESS_ID`/`DEVICE_ID` and `NAME` aren't set, a process ID and a name with it appended, so that it is a separate device. It picks the same ones each time it starts (they depend on its name), so its saved state is kept. To choose them yourself, set `NAME`, `PROCESS_ID` and `ALT_PORT` (at least 10 apart), see [Settings](#settings). PCMs with the same device ID in one application are one device.

**Resetting a device:** subscriptions, channel names and multicast flows are saved in `~/.local/state/inferno_aoip/<device ID>`; delete that directory while the device isn't open. `inferno-doctor` shows the path.

**Without the installer:** copy `target/release/libasound_module_pcm_inferno.so` into alsa-lib's plugin directory (`$(pkg-config --variable=libdir alsa)/alsa-lib`) and add the device from [`alsa_pcm_inferno/asoundrc`](alsa_pcm_inferno/asoundrc) to `~/.asoundrc`. Run Statime with `sudo statime -c inferno-ptpv1.toml`, after setting the network interface in that file.

## Cross compiling
If you want to use Inferno on Raspberry Pi or other single-board computer, it is usually faster to compile on your desktop/server/laptop (*host*) and copy resulting binaries to the *target*. Also, you will not run out of SD card space or RAM.

Fortunately, there is **actually zero-setup** tool for this: [`cross`](https://crates.io/crates/cross). Install it and instead of executing `cargo build`, do:

```
cross build --release --target=aarch64-unknown-linux-gnu
```

and you'll find compiled binaries in `target/aarch64-unknown-linux-gnu/release`.

Currently the only dependency needed on the target system is the ALSA library, if using `alsa_pcm_inferno`. Everything else is stored in the binary.

Installing dependencies on your host system is not needed - `cross` does it inside its container according to instructions from `Cargo.toml`.

If you need to compile for a different architecture, add it to `Cargo.toml` (copy-paste the `workspace.metadata.cross.target` section) before running `cross`. It is needed only for `alsa_pcm_inferno`. Statime does not have any shared library dependencies.


## Do you prefer Docker?
This won't work with usual desktop system usage, but if you are packaging an audio application using ALSA, into a Docker container, we've got you covered! See the directory `test/dockerized_trx` for inspiration (its Fedora image is installed with `install.sh --prebuilt`).

Remember that:
* Clock synchronization is still your responsibility. I didn't want to complicate the base image with service management so PTP daemon is not included. `usrvclock-rs` requires the sockets path (including client sockets) to be the same for all containers so a shared volume for `/tmp` is required. Future versions may include Statime as a separate container.
* Dante protocol requires good timing. Allow realtime priorities.
* Network routing (incl. multicast) may be tricky to configure correctly but `--network=host` is your friend. Or bridge the container with your AoIP network. NAT is your enemy.
* To have state persistence, mount `/root/.local/state/inferno_aoip` as a volume (or different user's if you application is not running as root)


# Legal and moral stuff
Disclaimer: Dante uses technology patented by Audinate. This source code may use these patents too. Consult a lawyer if you want to:
* make money of it
* distribute binaries in (or from) a region where software patents apply

This project makes no claim to be either authorized or approved by Audinate.

Please do not use this project to make counterfeit Dante devices/software, it is both immoral and illegal (while Audinate's approach is only immoral). Always specify that the implementation is unofficial and not endorsed by Audinate. Probably you can legally say that it is compatible with a subset of Dante protocol, but IANAL.

## License
This project is dual licensed under the GPLv3-or-later and AGPLv3-or-later. You may choose which license to use, or retain both. For example:

* if you want to integrate it into a project already licensed under the GPL, you have the rights to do so.
* if you want to fork it into something working in cloud (or public Internet in general), it will be beneficial to the Free Software Community to use the AGPL **and remove** GPL when forking.


# Tested with
## Dante devices
* Audinate AVIO
  * AVIO-AES3
  * AVIO-DAI2
  * AVIO-DIOUSBC
* Ben & Fellows 523019 4x4 balanced analog I/O module (based on Dante UltimoX4)
* MUSIC Group
  * Klark Teknik DN32-DANTE (in Behringer X32) (based on Dante Brooklyn II)
  * Behringer Aoip-Dante (in Behringer Wing-Rack) (based on Dante Brooklyn III)
* Orban Optimod 5750 (based on Dante Broadway)
* Soundcraft
  * Vi2000
  * Vi3000
* Allen&Heath
  * SQ-5
  * SQ-6
  * Qu-5D
* Yamaha
  * Dante-MY16-AUD (in Yamaha LS9) (also tested with old 3.x Dante firmware version)
  * RIVAGE PM3
  * DSP-RX
* ESI planet 22c
* Dante Via @ OS X and Windows 11
* Dante Virtual Soundcard @ Windows 10

## Control software
* Dante Controller @ Windows 10, 11
* network-audio-controller

## Host
* x86_64 Linux
  * Arch
  * Ubuntu
  * Fedora
* aarch64 (ARM 64-bit) Linux
  * Raspberry Pi 5 - Raspberry Pi OS & Armbian Bookworm
  * Raspberry Pi 4 (no hardware PTP) - Raspberry Pi OS Lite (64bit)
  * Raspberry Pi Zero 2 W (with USB Ethernet) - Raspberry Pi OS Lite (64bit) - CAUTION build times are long



# Anatomy of the repository
* `inferno_aoip` - main library crate for emulating a Dante audio over IP device. In the future controller functionality will also be implemented. **Start here if you want to develop your app based on Inferno**.
* `inferno2pipe` - capture audio, writing interleaved 32-bit integer samples into an Unix named pipe (or a raw file). Helper script for recording to more convenient format is also provided. **Start here if you want to use Inferno for capturing audio without setting up whole audio stack**
* `inferno_driver_core` - platform-neutral core of virtual soundcard drivers: the device shared by an application's streams, and the stream lifecycle (prepare, start, stop, restart without dropping flows, waiting for the clock). **Start here if you want to write a driver for another audio API** (CoreAudio, Windows): translate its callbacks to a `Stream`'s, like `alsa_pcm_inferno` does
* `pipe2inferno` - the opposite of `inferno2pipe`: transmits interleaved 32-bit integer samples from a file or a pipe (`-` for stdin). For testing, and for sending audio without an audio stack
* `asio_inferno` - ASIO driver: a virtual soundcard for Windows DAWs and other ASIO hosts (experimental, see [its README](asio_inferno/README.md))
* `alsa_pcm_inferno` - virtual soundcard for ALSA. **Start here if you've ever dreamed of Dante Virtual Soundcard for Linux**
* `searchfire` - fork of [Searchlight](https://github.com/WilliamVenner/searchlight) mDNS crate, modified for compatibility with Dante's mDNS

`inferno_aoip`, `inferno_driver_core`, `inferno2pipe` and `pipe2inferno` build for Linux, macOS and Windows; the ALSA plugin and `inferno-doctor` are Linux-only, the ASIO driver Windows-only. The media clock comes from a `ClockSource` (`inferno_aoip::clock`), chosen by [`CLOCK_SOURCE`](#settings): on Unix the default is a PTP daemon at `CLOCK_PATH` (see below), on Windows Inferno's own PTPv1 follower. Applications can also give a device their own with `DeviceServer::try_start_with_clock` (or `Stream::with_clock_source`).


# Clocking options

## Software timestamping
It is the default option, compatible with all NICs (network cards) and Dante devices. It requires [our fork or the Statime daemon](https://github.com/teodly/statime/tree/inferno-dev).

Change the network interface in `inferno-ptpv1.toml` configuration file and run the daemon using:

```
sudo target/debug/statime -c inferno-ptpv1.toml
```

If you want extra stability, disable time synchronization, usually one of the following commands will suffice:
```
sudo systemctl stop chronyd.service
sudo systemctl stop systemd-timesyncd.service
sudo systemctl stop ntpd.service
```
This should no longer be needed if `virtual-system-clock-base` is set to `monotonic` or anything that starts with `monotonic`, but this fix/workaround is very young so take care.

If you have high enough `TX_LATENCY_NS` and `RX_LATENCY_NS` configured (TODO: how high is enough?), you can try to use `monotonic_coarse` which should reduce CPU load.

You can change the protocol from PTPv1 to PTPv2 in Statime configuration - this allows master operation (currently implemented only for PTPv2) so Inferno can be used even when no physical Dante device is in the network. But then at least one Dante device with AES67 enabled must be present in the network to make Inferno and Dante devices interoperate.

## Hardware timestamping
If your network card supports hardware timestamping (check with `ethtool -T`), you can use its clock directly without relying on system clock. It is more accurate than software timestamping, meaning that you can potentially use lower audio latencies.

Set the Inferno's configuration option `CLOCK_PATH` to PTP clock device path, usually `/dev/ptp0`.

If you require PTPv1 (Dante without AES67), you need to use our fork of Statime because there is no other open source PTPv1 daemon. In `inferno-ptpv1.toml`, set the `hardware-clock` option to `auto` and start the daemon:
```
sudo target/debug/statime -c inferno-ptpv1.toml
```

If you can live with PTPv2, you can use different PTP implementation, e.g. [linuxptp](https://www.linuxptp.org/):
```
sudo ptp4l -i enp0s31f6 -p /dev/ptp0 -l6 -E -H -s -m -4 --priority1 255 --priority2 255 --domainNumber 0 --freq_est_interval=7 --delay_filter_length=240 --dscp_event 46
```

or [unpatched (upstream) Statime](https://github.com/pendulum-project/statime).


Audio packets latency jitter is similar when using Statime and ptp4l, however clock histogram in DC shows that ptp4l is doing more abrupt changes in the long run and Statime is unstable for a few seconds during startup.


Be aware that Inferno queries the clock pretty often, which may put more load on the CPU than using system clock. If you want to avoid it but still have some benefits of hardware timestamping, specify `hardware-clock` in Statime config, but leave Inferno's `CLOCK_PATH` unset. Also read the [Software timestamping](#software-timestamping) section then.

# Configuration
Configuration can be set via:

* ALSA plugin configuration - in the `pcm` device definition (e.g. `/etc/alsa/conf.d/50-inferno.conf` or `~/.asoundrc`), or as arguments of the device name (`inferno:RX_CHANNELS=8`); prefer the definition, because too long ALSA device strings may be truncated (happens with PipeWire, not sure about other apps)
* environment variables - add `INFERNO_` prefix to the setting name, e.g. `INFERNO_BIND_IP=eth0`

The ALSA configuration wins over environment variables. Names are case-insensitive. An invalid value fails opening the device with a message saying what is accepted (instead of crashing the application); unknown names are reported as warnings (misspelt?). `inferno-doctor` checks the settings an application would get.

## Settings
All settings have default values. In a simple setup you should be able to start without specifying any parameters.
* `BIND_IP` - which local IPv4 address to bind to. Specifying it may be necessary if you have multiple network interfaces. Alternatively, network interface name may be specified, in that case the first IPv4 address belonging to the interface will be used. Defaults to the address of the IPv4 default route or, without one, an interface's IPv4 address (see [IPv4 and IPv6](#quick-start)).
* `DEVICE_ID` - 16 hexadecimal digits (8 bytes) used as a device ID. Dante devices usually use MAC address padded with zeros. Inferno uses `0000<IP address>0000` by default. Device ID is the storage key when saving state.
* `NAME` - name of advertised device. If unspecified, name based on app name and IP address will be generated. May be removed in future versions when it becomes settable from DC and so stored in a configuration file.
* `SAMPLE_RATE` - sample rate this device will operate on
* `PROCESS_ID` - integer number between 0 and 65535, part of the default `DEVICE_ID`. Defaults to 0, or, if the device picks its own ports (see `ALT_PORT`), a number derived from them. When setting `ALT_PORT` for several instances on a single IP address, set a unique `PROCESS_ID` too: specifying different `DEVICE_ID`s is not sufficient. Devices with the same `DEVICE_ID` (by default: IP address and `PROCESS_ID`) opened in one process are the same device, with the settings of the first one opened.
* `ALT_PORT` - start of the range of UDP ports used by socket listeners. If not specified, standard Dante ports as seen in hardware devices will be used, or, if they are in use by another device on this host, free ports from 20000-20999. Currently 4 ports are used (`ALT_PORT` to `ALT_PORT+3`) but it may change in the future, so better separate different instances by at least 10 ports.
* `RX_CHANNELS` - number of receive channels, defaults to 2. The ALSA plugin offers exactly this many capture channels; applications built on `inferno_aoip` may set their own count.
* `TX_CHANNELS` - number of transmit channels, defaults to 2. The ALSA plugin offers exactly this many playback channels.
* `RX_LATENCY_NS` - receive latency in nanoseconds, i.e. how much time to wait for media packets, relatively to PTP media clock. Equivalent to latency setting in Dante Controller. Defaults to 10ms. May be removed in future versions when it becomes settable from DC and so stored in a configuration file.
* `TX_LATENCY_NS` - transmit latency in nanoseconds, i.e. receive latency that this device will demand from devices receiving from us. Equivalent to latency setting in Dante Virtual Soundcard. Defaults to 10ms.
* `TX_SOURCE_BIT_DEPTH` - bit depth of the audio applications transmit (16, 24 or 32, default 32). When it's higher than the bit depth sent over the network (24 bits), the transmitted audio is dithered. Set it to 24 for bit-exact transmission of 24-bit audio.
* `CLOCK_SOURCE` - where the media clock comes from: `usrvclock` (a PTP daemon such as Statime at `CLOCK_PATH`; the default on Linux and macOS), `ptpv1` (Inferno's own PTPv1 follower on the `BIND_IP` interface, UDP 319/320; the default on Windows) or `free-running` (the local clock, not synchronised to the network: only for devices on one host). The PTPv1 follower timestamps in software and needs a PTPv1 leader on the network (a Dante device); on Windows, allow UDP 319 and 320 through the firewall.
* `CLOCK_PATH` - path to either [usrvclock](https://gitlab.com/lumifaza/usrvclock) socket (default `/tmp/ptp-usrvclock`, where Statime creates it), or PTP device. If the latter, make sure [you are allowed](https://gitlab.freedesktop.org/pipewire/pipewire/-/blob/78642cc53bd84c2ad529f2175cc50a658d1e52c0/src/daemon/90-pipewire-aes67-ptp.rules) to read it. Also, write permissions are required if you want to see actual frequency offset in DC. (the clock is never adjusted from within Inferno, but the syscall to read the frequency requires write access)
* `USE_SAFE_CLOCK` - not supported yet, must be `false` (the default).

ALSA plugin only:
* `DISABLE_POLLFD` - `1` disables the file descriptor the plugin uses to wake applications after each period. Some applications (e.g. SoX) don't work without it, so it is enabled by default.
* `USE_FLOWS_CLOCK` - `1` reports the capture position from the receiver's progress instead of the media clock.


# Audio stability tips

## Allow realtime priority
Inferno's audio threads ask for realtime (SCHED_FIFO) priority 80-81 and warn if they don't get it; without it, other load causes dropouts. Allow it for the user (or a group like `audio`) running the audio application, e.g. in `/etc/security/limits.conf`:

    @audio - rtprio 95
    @audio - memlock unlimited

then log in again. PipeWire users usually get it via RTKit or the `pipewire` group. `inferno-doctor` checks it.

## Set appropriate latency

Disable Inferno. Run cyclictest for at least 20 minutes, while doing things that you normally do with the computer:

    cyclictest --mlockall --smp --priority=80 --interval=5000 --distance=0

You can also add some artificial system load:

    stress -m `nproc` -c `nproc`

In cyclictest output, the last column is the worst-case latency in microseconds. Multiply it by 1000, add some margin and time for network transfer and you get the lowest safe value for `TX_LATENCY_NS` and `RX_LATENCY_NS`. If the values are higher than your acceptable latency, you will have hard time using Inferno (and computer audio in general). Also keep in mind that the maximum latency that Dante officially allows is 40ms. You can try to follow tutorials to make latency more predictable:

* [Professional audio @ Arch wiki](https://wiki.archlinux.org/title/Professional_audio) (not just for Arch)
* [Performance tuning @ PipeWire wiki](https://gitlab.freedesktop.org/pipewire/pipewire/-/wikis/Performance-tuning)
* [Fedora Pipewire Low Latency Audio Configuration Reference Guide](https://linuxmusicians.com/viewtopic.php?t=27121) (Fedora-specific)
* isolate CPU cores using `isolcpus` [or `cset`](https://unix.stackexchange.com/a/692558) and pin audio processes and network card IRQ to the isolated cores
* if nothing else helps, try `PREEMPT_RT` kernel


# Contributing
Issue reports and pull requests are welcome.

By submitting any contribution, you agree that it will be distributed according to the comment found at the top of `inferno_aoip/src/lib.rs` file - under the terms of GNU GPL v3 or any later version, or GNU AGPL v3 or any later version.

If you want to fork this project into something working in cloud (or public Internet in general), you may consider removing the GPL license and retaining AGPL.

Please use editor respecting `.editorconfig` (for example, VSCode needs an extension: [EditorConfig for VS Code](https://open-vsx.org/extension/EditorConfig/EditorConfig)) or configure it approprietly manually.

## Dependencies
This project is dependent on Statime ([upstream](https://github.com/pendulum-project/statime), [our fork](https://github.com/teodly/statime/tree/inferno-dev)), so if you want to help, look at its TODO list, too! The most important are:
* [PTPv1 support](https://github.com/pendulum-project/statime/pull/602) - slave already working but not upstreamed yet
* [PHC-only mode](https://github.com/pendulum-project/statime/issues/517) (for hardware timestamping)
* [Independent clock for arbitrary timescale](https://github.com/pendulum-project/statime/issues/389) (for software timestamping) - already sort of working, but unstable due to Linux using CLOCK_REALTIME for software timestamps, and not upstreamed completely

## Non-code contributions
If you want to contribute but can't code or don't know where to start because this project is complex, you can:
* report issues
* request new features
* discuss new features, often a decision needs to be made and having multiple voices helps
* test with more Linux software! `grep -r 'not tested yet'` for a list
* try to achieve sub-millisecond latency with PREEMPT_RT kernel
* test with more [Dante devices](#dante-devices), preferably very new or very old ones without firmware updates


# Changelog

## Unreleased
* DAW support in the ALSA plugin: stopping and restarting a stream (transport stop/start, xrun recovery) keeps the Dante device, flows and subscriptions running, audio resumes within about the network latency instead of several seconds
* `snd_pcm_delay` includes the network latency, for latency compensation in DAWs
* the device transmits silence while no application is playing (e.g. capture-only applications), so receivers don't see a broken stream; starting or stopping playback switches the transmitter without interrupting flows
* changing the buffer configuration (e.g. a DAW changing its buffer size) switches the receiver and transmitter to the new buffers, keeping flows and subscriptions
* capture: buffers too small for the receive latency are refused, overwritten samples are reported as xruns instead of being returned
* playback: samples are no longer overwritten before they're transmitted, and silence (not the stale buffer) is transmitted when the application falls behind or stops
* receiving: no audio from the previous buffer cycle while a transmitter stalls, late packets (e.g. queued while the receiver was stalled) are dropped instead of landing on positions about to be read
* answers controllers' clock status, settings status (AES67, sample rate, encoding, pull-up, codec, lock) and interface statistics queries, so that e.g. netaudio doesn't wait for them to time out; fixed heartbeat record length
* heartbeats report late packets per receive flow (cumulative), like Dante hardware, as the advertised late packet monitoring capability promises
* setup: `install.sh` installs the ALSA plugin and device (optionally PipeWire nodes and the Statime clock service); `inferno-doctor` checks the setup and says what to fix
* invalid settings fail opening the device with a message naming the setting, instead of crashing the application; misspelt settings are warned about; the ALSA plugin's own settings also work as `INFERNO_*` variables
* a missing clock is reported, with what is wrong at `CLOCK_PATH`; a device that can't get its network ports (e.g. used by another device) fails to start with an error naming them, instead of running invisible to controllers
* the ALSA plugin and Inferno2pipe log at `info` by default (was `debug`)
* several devices on one computer without configuring them: a device whose standard ports are in use picks free ports, a process ID and a name, the same ones each time; multicast flows can be received by several devices (or other software) on one host; a PCM merged into a device with other settings (same device ID in one process) is warned about
* dual-stack hosts: without an IPv4 default route (e.g. internet over IPv6 only), the device uses an interface's IPv4 address instead of failing to start; an IPv6 `BIND_IP` gets an explanation (Dante uses IPv4)
* the `inferno` ALSA device is listed by `aplay -L` and in applications' device lists: its arguments default to empty, and empty settings count as unset
* receive channels are silenced when their flow is removed (unsubscribed, transmitter gone) even if the flow's socket is removed first, instead of keeping the last audio; no more "BUG: still have N channels" error
* draining playback (e.g. aplay at the end of a file) is no longer logged as an xrun
* RX channels are listed even without a running receiver (playback-only devices weren't usable from netaudio 0.4)
* warning when realtime priority can't be set for the audio threads
* integration test: netaudio 0.4, runs on arm64 and x86, exits with the test result, configurable latency (`INFERNO_TEST_LATENCY_NS`)
* portability: `inferno_aoip` builds for macOS and Windows. The media clock comes from a `ClockSource` (usrvclock/Statime by default on Unix), and applications can give a device their own with `DeviceServer::try_start_with_clock`
* new `inferno_driver_core` crate: the shared device and stream lifecycle, moved out of the ALSA plugin, to build drivers for other audio APIs on
* new `pipe2inferno`: transmits audio from a file or pipe
* new ASIO driver for Windows (`asio_inferno`, experimental): tested in CI with a minimal ASIO host, Pipe2inferno -> driver inputs and driver outputs -> Inferno2pipe, bit-exact
* PTPv1 follower built in (`CLOCK_SOURCE=ptpv1`, the default on Windows): follows the clock of a Dante network without a PTP daemon. Tested against a simulated leader on Linux, macOS and Windows, and in the Windows end-to-end test
* Windows: receiving multicast flows works (sockets can't be bound to a multicast address there); end-to-end test in CI (`test/windows_e2e`): Pipe2inferno to Inferno2pipe routed with netaudio, bit-exact
* a device that can't listen on its address gets a hint matching the cause (address in use, or not an address of this host)
* Pipe2inferno reads its input along the media clock: no more gaps of silence after the first subscription
* Windows: the audio threads (transmit, receive, PTP) register with MMCSS ("Pro Audio") instead of a thread priority below the audio threads of hosts and drivers
* the transmitter no longer sleeps 2 ms and skips flows when catching up after a stall
* external buffers' validity is an atomic flag: real-time threads no longer take a lock, which could fail spuriously (dropping a packet's samples, or marking the previous ring cycle's samples readable)
* the ALSA plugin wakes the application about 4 times per period instead of on every packet
* receiving: writing in-order packets into a ring buffer is about 10x faster (no per-sample bookkeeping without holes)
* ASIO driver: a buffer switch finishing after its outputs were due is warned about (the host has one period, as with any ASIO driver)

## 0.5.0
* works with JACK, should work with any app that uses the soundcard as an interrupt source
* introduced automated integration test that launches several instances, streams audio data between them and checks for its correctness (including audio comparison). Currently tests JACK, PipeWire, aplay, arecord with alsa_pcm_inferno. SoX and Inferno2pipe are planned.
* Dockerfile
* fix logic error in CPU lockup prevention code

## 0.4.3
* support monotonic clock as base with jump-preventing guard (Statime)
* reduced snowball effect when CPU is overloaded, transmitter is lagging and tries to transmit multiple packets at once: max lag is set to `TX_LATENCY_NS`, and if transmit thread is locked for more than 5ms, 2ms sleep will be forced

## 0.4.2
* optimization: remove modulo division from ringbuffer index calculations

## 0.4.1
* disable SafeClock for now, doesn't help much and harms PHC
* fix panic on devices that can tx high number of channels per flow (e.g. Behringer Wing)

## 0.4.0
* refactor - preparation to introduction of controller functionality
* multicast transmitter

## 0.3.3
* fix starting without default route in routing table (Searchfire)

## 0.3.2
* changeable RX & TX channel names
* RX & TX latency configurable via env var or ALSA plugin parameter
* relicense to GPL-or-AGPL to simplify forking to AGPL projects, as parts of this project may be useful in cloud applications

## 0.3.1
* read configuration from ALSA plugin parameters - useful for multiple sources & sinks in PipeWire
* send statistics (clock, latency, signal levels)
* changeable usrvclock path

## 0.3.0
* introduced ALSA PCM plugin - a virtual soundcard compatible with most Linux audio apps
* receive clock using a documented protocol: [usrvclock](https://gitlab.com/lumifaza/usrvclock)
* various internal changes primarily related to allowing the use of external buffers (needed for mmap mode in ALSA plugin)
* receive multicasts
* ability to use non-default network ports to allow running multiple instances on a single IP address
* removed Inferno Wired because the ALSA plugin works well with PipeWire. [This is the last version](https://gitlab.com/lumifaza/inferno/-/blob/3941765700696f545425a5479be25091fda514d4/inferno_wired/src/main.rs) for the curious.

## 0.2.0
* audio transmitter
* alpha version of Inferno Wired - virtual audio source & sink for PipeWire
* receiving clock from [Statime](https://github.com/teodly/statime) modified for PTPv1 and virtual clock support - Linux-only for now (because CLOCK_TAI is Linux-only)
* increased receive thread priority to reduce chance of OS UDP input queue overflow

## 0.1.0

initial release


# To do
likely in order they'll be implementated

* ability to change settings in Dante Controller

At this point, Inferno will roughly become alternative to Dante Virtual Soundcard.

* realtime thread priorities configurable
* verify the late packets reported in heartbeats show up in Dante Controller
* operation without PTP daemon with lower clock precision - useful for OSes other than Linux
* read configuration from text files
* ability to work as a clock source (PTPv1 leader) - Statime
  * it is already possible if you use PTPv2 - in theory you should be able to make Inferno-only AoIP network - not tested yet
* add more apps and Linux distributions to the integration test
* bit-perfect transmitter (32-bit integers are always used internally; TX dithering for 16-bit and 24-bit output is configurable via `TX_SOURCE_BIT_DEPTH`)
* command line helper / TUI / GUI
* installer support for cross-compiled builds (`install.sh --prebuilt` installs a plugin built elsewhere)
* more refactoring of network packets serializers & deserializers to make more code reusable in upcoming controller app (because DC is bloated closed source and bad UX)
* AES67
* primary & secondary network support, for dual-NIC computers
* API: number of channels changeable without device server restart (useful for Dante Via-like operation where transmitters & receivers can be added and removed dynamically)
  * not really necessary to replicate functionality of Via, as now multiple Inferno instances on a single IP address are supported
* `grep -r TODO inferno_aoip/src`


# Design
* 99% safe Rust (unsafe is required only because ALSA plugin API doesn't have safe Rust bindings)
* no external libraries needed, the only dependencies are Rust crates


# Motivation
I've been using free as in freedom, open source software for many years now. I'm also fascinated by connections between music and technology. One day my sound engineer collegue showed me how Dante works, how easy to use and (most of the time) stable it is. The problem was that it's not an open standard, didn't have open source implementation and I couldn't use it on my favourite operating system - Linux. Now I can.

## Why not AES67?
* AES67 is only a standard of media transport, not control, so flows need to be established manually. NMOS could fix it but I doubt Audinate will implement it in reasonable future.

And the following are limitations of AES67 implementation in Dante, not AES67 in general:

* it is not supported in Dante Virtual Soundcard and Dante Via
* some older Dante devices (without firmware upgrades) don't support it either
* it is multicast-only
* sample rate is locked to 48kHz

# Other open source projects related to Dante
* [network-audio-controller](https://github.com/chris-ritsen/network-audio-controller) - command line connection and device controller, alternative to Dante Controller
* [companion-module-audinate-dantecontroller](https://github.com/bitfocus/companion-module-audinate-dantecontroller) - Bitfocus Companion module for controlling Dante devices
* [inferno_runners](https://github.com/maze42d/inferno_runners) - scripts for running Inferno with PipeWire-based inter-soundcard bridge and [USB audio gadget](https://www.diyaudio.com/community/threads/linux-usb-audio-gadget-rpi4-otg.342070/)
* [dante-aes67-relay.js](https://gist.github.com/philhartung/87d336a3c432e2ce5452befcad1b945f) - Relay a Dante multicast stream to AES67
* [wycliffe](https://github.com/jsharkey/wycliffe), receiver implementation contained in a video control software - the earliest public attempt of reverse engineering Dante
* [List of AES67 audio resources](https://aes67.app/resources) at [AES67 Stream Monitor](https://aes67.app/) website (Dante is AES67-compatible but not on all devices and requires manual configuration)

## Alternatives
To my knowledge, there are no other unofficial implementations of audio transmission compatible with Dante. However, if AES67 fits your use case, you may want to use:
* [AES67 Linux daemon](https://github.com/bondagit/aes67-linux-daemon) with kernel-level virtual soundcard
* [pipewire-aes67](https://gitlab.freedesktop.org/pipewire/pipewire/-/wikis/AES67)
* [aes67-recorder](https://github.com/voc/aes67-recorder) ([tutorial for usage with Dante](https://behringer.world/viewtopic.php?t=197))
* AES67 can be also received by any software that supports raw audio in RTP packets, e.g. FFmpeg or GStreamer.
  * ... Dante multicasts are equally simple... just cut off 9 bytes at the front of every UDP packet ;)
