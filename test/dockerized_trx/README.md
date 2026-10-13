Tests Inferno audio over IP transfers in different conditions:
* apps using Inferno library, directly or indirectly
* Linux distributions

Uses [WavDiff from diff_tools](https://github.com/aspt/diff_tools) for sample-perfect comparisons.

## How to use

Run `run_test.sh`, it handles everything including building Docker images. It exits with the test result;
set `ARTIFACTS_DIR=<dir>` to keep the recordings and reports.

Each `alpine_aplay_arecord` container runs two Inferno devices on one host (aplay and arecord, separated by
`PROCESS_ID` and `ALT_PORT`). The capture device binds to a second address (`CAPTURE_IP`) on the same interface,
because netaudio 0.4 keys devices by IP. `audio_net` therefore uses a fixed subnet,
`10.213.77.0/24` by default; set `AUDIO_NET_PREFIX` (e.g. `AUDIO_NET_PREFIX=10.99.1`) if it clashes with
a local network.

**PENDING:** two devices on the *same* IP (`aplayrec3`, routed `aplayrec3p` -> `aplayrec3c`) are tested only with
`INFERNO_TEST_SAME_HOST_DEVICES=1 ./run_test.sh`. netaudio 0.4.2 lists and subscribes them, but reports
"control address ... is ambiguous across devices", skips their status probes, and the slower discovery
makes the run fail with "wasted too much time". Enable this by default once netaudio supports multiple
devices per host.

Timing: netaudio 0.4 waits for several probes Inferno doesn't answer, so one `netaudio device list` pass takes
20-30 s, and a device that starts late may need a second pass. Hence `DURATION=120` in `inferno.env`: discovery
gets `DURATION/2`, and the minimal signal duration counts from the controller's start, discovery included.

Inferno containers get `CAP_SYS_NICE` and `rtprio`, so its audio threads run with realtime priority (Inferno logs
"unable to set realtime priority" otherwise).

Latency: every device uses `INFERNO_TEST_LATENCY_NS` (default 40 ms, Dante's maximum). It must exceed the host's
worst-case scheduling latency: Docker Desktop's VM is paused by the host OS now and then (measured up to ~20 ms on
macOS), which realtime priority inside the VM can't prevent. On a tuned Linux host you can test tight latency, e.g.
`INFERNO_TEST_LATENCY_NS=10000000 ./run_test.sh`; measure first as described in
[Audio stability tips](../../README.md#audio-stability-tips).

It may still spuriously fail due to realtime scheduling issues, a.k.a. xruns (including in the test's own JACK
recorders). In that case try again or [tune your system](../../README.md#audio-stability-tips).
