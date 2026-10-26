End-to-end test of Inferno on Windows, run by `.github/workflows/windows-e2e.yml`.

[`pipe2inferno`](../../pipe2inferno) transmits 150 s of pseudo-random 24-bit audio, [`inferno2pipe`](../../inferno2pipe)
receives it, routed with [netaudio](https://github.com/chris-ritsen/network-audio-controller) like the Linux
[Docker test](../dockerized_trx). The recording must match the source exactly (bit for bit) for at least 15 s.

Both devices run on one host, each following the clock of a simulated PTPv1 leader (the `ptpv1_test_leader`
example, standard PTP ports) with its own PTPv1 follower: Inferno's default clock source on Windows. This tests
Inferno's Windows-specific parts (sockets, multicast, mDNS next to Windows' own responder, PTP timestamps,
timing). It doesn't test interoperability with Dante hardware.

netaudio 0.4 keys devices by IP, so the workflow runs it twice: with each device on its own address of a Microsoft
KM-TEST loopback adapter (installed with devcon), and with both on the runner's address (netaudio then lists and
subscribes them, but skips their status probes). Don't add addresses to the runner's own adapter: that cuts the
runner off from GitHub.

Run it yourself (Python 3, `pip install netaudio`, two addresses of this host):

    cargo build -p pipe2inferno -p inferno2pipe --example ptpv1_test_leader
    python test/windows_e2e/run_e2e.py --bin-dir target/debug --tx-ip <address> --rx-ip <address> --out-dir e2e-out
