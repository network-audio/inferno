Tests Inferno audio over IP transfers in different conditions:
* apps using Inferno library, directly or indirectly
* Linux distributions

Uses [WavDiff from diff_tools](https://github.com/aspt/diff_tools) for sample-perfect comparisons.

## How to use

Run `run_test.sh`, it handles everything including building Docker images. It exits with the test result;
set `ARTIFACTS_DIR=<dir>` to keep the recordings and reports. CI runs it on every push to `dev` and every pull request (`docker-suite` in
`.github/workflows/ci.yml`), with the Debian image for amd64: GitHub's runners can't resolve names in the i386 build.

Each `alpine_aplay_arecord` container runs two Inferno devices on one host (aplay and arecord, separated by
`PROCESS_ID` and `ALT_PORT`). In `aplayrec1` and `aplayrec2` the capture device binds to a second address
(`CAPTURE_IP`) on the same interface; in `aplayrec3` both devices share one IP (routed `aplayrec3p` ->
`aplayrec3c`), which netaudio tells apart since 0.5. `audio_net` uses a fixed subnet for the second addresses,
`10.213.77.0/24` by default; set `AUDIO_NET_PREFIX` (e.g. `AUDIO_NET_PREFIX=10.99.1`) if it clashes with a local
network.

Timing: devices run for `DURATION` seconds (`inferno.env`). Discovery gets `DURATION/2`, and the minimal signal
duration counts from the controller's start, discovery included.

Inferno containers get `CAP_SYS_NICE` and `rtprio`, so its audio threads run with realtime priority (Inferno logs
"unable to set realtime priority" otherwise).

Latency: every device uses `INFERNO_TEST_LATENCY_NS` (default 40 ms, Dante's maximum). It must exceed the host's
worst-case scheduling latency: Docker Desktop's VM is paused by the host OS now and then (measured up to ~20 ms on
macOS), which realtime priority inside the VM can't prevent. On a tuned Linux host you can test tight latency, e.g.
`INFERNO_TEST_LATENCY_NS=10000000 ./run_test.sh`; measure first as described in
[Audio stability tips](../../README.md#audio-stability-tips).

It may still spuriously fail due to realtime scheduling issues, a.k.a. xruns (including in the test's own JACK
recorders). In that case try again or [tune your system](../../README.md#audio-stability-tips).
