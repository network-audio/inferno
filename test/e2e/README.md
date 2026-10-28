End-to-end tests of Inferno on Windows and macOS, run by `.github/workflows/windows-e2e.yml` and
`.github/workflows/macos-e2e.yml`.

[`pipe2inferno`](../../pipe2inferno) transmits 150 s of pseudo-random 24-bit audio, [`inferno2pipe`](../../inferno2pipe)
receives it, routed with [netaudio](https://github.com/chris-ritsen/network-audio-controller) like the Linux
[Docker test](../dockerized_trx). The recording must match the source exactly (bit for bit) for at least 15 s without a break. Elsewhere it may only have silence or skips (where a stalled CI runner made audio late), after which it must continue the source exactly again: wrong audio, e.g. corrupted, repeated or stale, fails.

Both devices run on one host, each following the clock of a simulated PTPv1 leader (the `ptpv1_test_leader`
example, standard PTP ports) with its own PTPv1 follower: Inferno's default clock source on Windows. This tests
Inferno's Windows-specific parts (sockets, multicast, mDNS next to Windows' own responder, PTP timestamps,
timing). It doesn't test interoperability with Dante hardware.

netaudio 0.4 keys devices by IP, so the workflow runs it twice: with each device on its own address of a Microsoft
KM-TEST loopback adapter (installed with devcon), and with both on the runner's address (netaudio then lists and
subscribes them, but skips their status probes). Don't add addresses to the runner's own adapter: that cuts the
runner off from GitHub.

Run it yourself (Python 3, `pip install netaudio`, two addresses of this host):

    cargo build -p pipe2inferno -p inferno2pipe
    cargo build -p inferno_aoip --example ptpv1_test_leader
    python test/e2e/run_e2e.py --scenario pipe --bin-dir target/debug --ips <address>,<address> --out-dir e2e-out

The `soundcard` scenarios test the virtual sound cards (`inferno_soundcard`) with a minimal application of the
operating system's audio API: on Windows the `wasapi_host` example in exclusive mode, the cards attached by
usbip-win2; on macOS the `coreaudio_host` example, with the sound card service and its Core Audio plug-in
installed from the package (`packaging/macos/build-pkg.sh`) and `--installed-soundcard`: the script restarts the
service with its own settings (with `sudo`), and the application needs the microphone permission
(`grant_microphone.py`, on CI runners).
