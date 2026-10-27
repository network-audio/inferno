"""End-to-end tests of Inferno on Windows, routed with netaudio like the Linux Docker test
(test/dockerized_trx). Recordings must match their sources exactly. Exits with the test result.

Scenarios:
* pipe: Pipe2inferno transmits a known signal, Inferno2pipe receives it
* asio: the ASIO driver, in a minimal ASIO host (the `asio_host` example): Pipe2inferno -> driver inputs,
  recorded by the host; the host plays another signal to the driver outputs -> Inferno2pipe
* soundcard: a virtual sound card (inferno_soundcard, attached through usbip-win2), played and recorded in
  exclusive mode by a minimal WASAPI application (the `wasapi_host` example): Pipe2inferno -> the card's
  microphone, recorded; another signal played to the card's speakers -> Inferno2pipe
* pipe-leader: as pipe, without the simulated leader: the devices are leader candidates (CLOCK_LEADER=auto)
  and one of them leads the clock

Devices run on this host, each on an IP address of its own if given several (netaudio 0.4 keys devices by
IP). Each follows the clock of a simulated PTPv1 leader (the `ptpv1_test_leader` example) with its own
PTPv1 follower, Inferno's default clock source on Windows (except in pipe-leader).

Usage: python run_e2e.py --scenario pipe --bin-dir target/debug --ips 10.213.77.1,10.213.77.2,10.213.77.3 --out-dir e2e-out
"""

import argparse
import array
import json
import os
import random
import subprocess
import sys
import time
from pathlib import Path

SAMPLE_RATE = 48000
CHANNELS = 2
# more environment for the devices, per scenario
DEVICE_ENV = {}
SOURCE_SECONDS = 150
# audio to record once subscribed
RECORD_SECONDS = 20
MIN_MATCHING_SECONDS = 15
DISCOVERY_TIMEOUT = 120
LATENCY_NS = 40_000_000
TX_NAME = "wine2etx"
RX_NAME = "wine2erx"
ASIO_NAME = "wine2easio"
CARD_DEVICE_NAME = "wine2ecard"
CARD_NAME = "Inferno E2E"


def log(message):
    print(message, flush=True)


def write_source(path, seed=1234):
    """Interleaved native-endian s32: pseudo-random 24-bit values (Dante carries 24 bits), so that any
    window of the source is unique and a recording can be aligned to it exactly."""
    count = SOURCE_SECONDS * SAMPLE_RATE * CHANNELS
    random_bytes = random.Random(seed).randbytes(3 * count)
    # little-endian s32 with the low byte zero (x86 and ARM Windows are little-endian)
    data = bytearray(4 * count)
    for i in range(3):
        data[i + 1::4] = random_bytes[i::3]
    path.write_bytes(data)
    samples = array.array("i")
    samples.frombytes(data)
    return samples


def device_env(name, ip, process_id):
    env = dict(os.environ)
    env.update({
        "INFERNO_NAME": name,
        "INFERNO_BIND_IP": ip,
        "INFERNO_PROCESS_ID": str(process_id),
        "INFERNO_SAMPLE_RATE": str(SAMPLE_RATE),
        "INFERNO_RX_LATENCY_NS": str(LATENCY_NS),
        "INFERNO_TX_LATENCY_NS": str(LATENCY_NS),
        # the source is 24-bit: transmit it bit-exact, not dithered
        "INFERNO_TX_SOURCE_BIT_DEPTH": "24",
        # gaps filled with silence: normal when a stream starts, a dropout later
        "RUST_LOG": env.get("RUST_LOG", "info,inferno_aoip::ring_buffer=debug"),
    })
    env.update(DEVICE_ENV)
    return env


def netaudio(*args, timeout=90):
    result = subprocess.run(["netaudio", *args], capture_output=True, text=True, timeout=timeout)
    return result.returncode, result.stdout + result.stderr


def wait_for_devices(names, deadline):
    while True:
        code, out = netaudio("device", "list")
        found = [name for name in names if name in out]
        log(f"netaudio device list (exit {code}): found {found}")
        if len(found) == len(names):
            return True
        if time.time() > deadline:
            log(out)
            return False
        time.sleep(2)


def compare(source, recording_path):
    """Number of frames of the recording matching the source exactly, after its leading silence."""
    recording = array.array("i")
    recording.frombytes(recording_path.read_bytes()[: (recording_path.stat().st_size // 4) * 4])
    frames = len(recording) // CHANNELS
    log(f"recorded {frames} frames ({frames / SAMPLE_RATE:.1f} s)")
    # channels may start a packet apart (each subscription starts on a packet of its own): from where all have
    first = next((i for i in range(frames)
                  if all(recording[i * CHANNELS + c] != 0 for c in range(CHANNELS))), None)
    if first is None:
        log("recording is silent")
        return 0
    # align: find the first recorded frame in the source (unique with pseudo-random samples)
    window = recording[first * CHANNELS:(first + 8) * CHANNELS]
    source_bytes = source.tobytes()
    offset = source_bytes.find(window.tobytes())
    while offset >= 0 and offset % (4 * CHANNELS) != 0:
        offset = source_bytes.find(window.tobytes(), offset + 1)
    if offset < 0:
        log(f"recorded audio (from frame {first}) not found in the source")
        log("first recorded frames: " + " ".join(f"{v & 0xffffffff:08x}" for v in recording[:16 * CHANNELS]))
        log("first source frames:   " + " ".join(f"{v & 0xffffffff:08x}" for v in source[:16 * CHANNELS]))
        return 0
    source_frame = offset // (4 * CHANNELS)
    log(f"recording frame {first} is source frame {source_frame}")
    matching = 0
    mismatches = 0
    first_mismatch = None
    for i in range(first, frames):
        s = source_frame + i - first
        if s * CHANNELS + CHANNELS > len(source):
            break
        if recording[i * CHANNELS:(i + 1) * CHANNELS] == source[s * CHANNELS:(s + 1) * CHANNELS]:
            matching += 1
        else:
            mismatches += 1
            if first_mismatch is None:
                first_mismatch = i
    log(f"matching frames: {matching}, mismatching: {mismatches}"
        + (f" (first at recorded frame {first_mismatch})" if first_mismatch is not None else ""))
    return matching if mismatches == 0 else 0


class Processes:
    """Child processes with their logs in the output directory, killed at the end."""

    def __init__(self, out):
        self.out = out
        self.processes = {}

    def start(self, name, command, env):
        log_file = open(self.out / f"{name}.log", "w")
        self.processes[name] = (subprocess.Popen(command, env=env, stdout=log_file, stderr=subprocess.STDOUT), log_file)
        return self.processes[name][0]

    def exited(self):
        return {name: p.poll() for name, (p, _) in self.processes.items() if p.poll() is not None}

    def stop(self, failed):
        for process, log_file in self.processes.values():
            if process.poll() is None:
                process.kill()
                process.wait()
            log_file.close()
        if failed:
            for name in self.processes:
                log(f"----- {name}.log (last lines)")
                log("\n".join((self.out / f"{name}.log").read_text(errors="replace").splitlines()[-60:]))


def subscribe(tx, rx):
    code, output = netaudio("subscription", "add", "--tx", tx, "--rx", rx)
    log(output)
    if code != 0:
        log(f"❌ subscription {tx} -> {rx} failed")
        return False
    return True


def check_subscribed(rx_names):
    code, output = netaudio("subscription", "list")
    log(output)
    for rx in rx_names:
        subscribed = [line for line in output.splitlines() if rx in line and "Subscribed" in line]
        if len(subscribed) < CHANNELS:
            log(f"❌ expected {CHANNELS} subscribed channels of {rx}")
            return False
    return True


def check_clock(log_paths):
    for path in log_paths:
        if "clock available from PTPv1" not in path.read_text(errors="replace"):
            log(f"❌ {path.name}: the device doesn't follow the PTPv1 leader")
            return False
    log("✅ all devices follow the PTPv1 leader")
    return True


def check_one_leader(log_paths):
    leading = [path.name for path in log_paths if "leading the clock as" in path.read_text(errors="replace")]
    if not leading:
        log("❌ no device leads the clock")
        return False
    log(f"✅ leading the clock: {', '.join(leading)}")
    return True


def check_recording(source, path, what):
    matching = compare(source, path)
    if matching < MIN_MATCHING_SECONDS * SAMPLE_RATE:
        log(f"❌ {what}: less than {MIN_MATCHING_SECONDS} s of the recording matches the source exactly")
        return False
    log(f"✅ {what}: {matching / SAMPLE_RATE:.1f} s of audio received exactly as transmitted")
    return True


def pipe_scenario(args, procs, exe):
    out = args.out_dir
    source = write_source(out / "source.raw")
    recording = out / "recording.raw"
    procs.start("tx", [args.bin_dir / f"pipe2inferno{exe}", "-c", str(CHANNELS), "-i", out / "source.raw"],
                device_env(TX_NAME, args.ips[0], 1))
    rx = procs.start("rx", [args.bin_dir / f"inferno2pipe{exe}", "-c", str(CHANNELS), "-o", recording],
                     device_env(RX_NAME, args.ips[1 % len(args.ips)], 2))
    start = time.time()
    if not wait_for_devices([TX_NAME, RX_NAME], start + DISCOVERY_TIMEOUT):
        log("❌ devices not discovered in time")
        return False
    log(f"✅ devices discovered ({time.time() - start:.0f} s)")
    if not check_clock([out / "tx.log", out / "rx.log"]):
        return False
    if args.scenario == "pipe-leader" and not check_one_leader([out / "tx.log", out / "rx.log"]):
        return False
    if not subscribe(TX_NAME, RX_NAME) or not check_subscribed([RX_NAME]):
        return False
    log("✅ subscribed")
    time.sleep(RECORD_SECONDS)
    if procs.exited():
        log(f"❌ a device exited early: {procs.exited()}")
        return False
    rx.kill()
    rx.wait()
    return check_recording(source, recording, "Pipe2inferno -> Inferno2pipe")


def asio_scenario(args, procs, exe):
    out = args.out_dir
    ip = lambda i: args.ips[i % len(args.ips)]
    to_asio = write_source(out / "to_asio.raw", seed=1234)
    from_asio = write_source(out / "from_asio.raw", seed=5678)
    asio_recording = out / "asio_recording.raw"
    rx_recording = out / "rx_recording.raw"
    stop_file = out / "stop-asio-host"
    procs.start("tx", [args.bin_dir / f"pipe2inferno{exe}", "-c", str(CHANNELS), "-i", out / "to_asio.raw"],
                device_env(TX_NAME, ip(0), 1))
    # the driver logs to its temporary directory: the output directory here
    asio_env = dict(device_env(ASIO_NAME, ip(1), 3), TMP=str(out), TEMP=str(out),
                    INFERNO_ASIO_CONFIG=str(out / "no-config-file"),
                    INFERNO_RX_CHANNELS=str(CHANNELS), INFERNO_TX_CHANNELS=str(CHANNELS))
    asio = procs.start("asio_host", [args.bin_dir / "examples" / f"asio_host{exe}", args.bin_dir / "inferno_asio.dll",
                                     "--play", out / "from_asio.raw", "--record", asio_recording,
                                     "--buffer", "256", "--seconds", "300", "--until-file", stop_file], asio_env)
    rx = procs.start("rx", [args.bin_dir / f"inferno2pipe{exe}", "-c", str(CHANNELS), "-o", rx_recording],
                     device_env(RX_NAME, ip(2), 2))
    start = time.time()
    if not wait_for_devices([TX_NAME, ASIO_NAME, RX_NAME], start + DISCOVERY_TIMEOUT):
        log("❌ devices not discovered in time")
        return False
    log(f"✅ devices discovered ({time.time() - start:.0f} s)")
    if not check_clock([out / "tx.log", out / "rx.log", out / "inferno_asio.log"]):
        return False
    if not subscribe(TX_NAME, ASIO_NAME) or not subscribe(ASIO_NAME, RX_NAME):
        return False
    if not check_subscribed([ASIO_NAME, RX_NAME]):
        return False
    log("✅ subscribed")
    time.sleep(RECORD_SECONDS)
    if procs.exited():
        log(f"❌ a device exited early: {procs.exited()}")
        return False
    # the recorder first: once the host stops, the driver transmits silence
    rx.kill()
    rx.wait()
    stop_file.write_text("stop")
    try:
        code = asio.wait(timeout=30)
    except subprocess.TimeoutExpired:
        log("❌ the ASIO host didn't stop")
        return False
    log((out / "asio_host.log").read_text(errors="replace"))
    if code != 0:
        log(f"❌ the ASIO host failed ({code})")
        return False
    ok = check_recording(to_asio, asio_recording, "Pipe2inferno -> ASIO driver inputs")
    return check_recording(from_asio, rx_recording, "ASIO driver outputs -> Inferno2pipe") and ok


def soundcard_scenario(args, procs, exe):
    out = args.out_dir
    ip = lambda i: args.ips[i % len(args.ips)]
    to_card = write_source(out / "to_card.raw", seed=1234)
    from_card = write_source(out / "from_card.raw", seed=5678)
    card_recording = out / "card_recording.raw"
    rx_recording = out / "rx_recording.raw"
    stop_file = out / "stop-wasapi-host"
    config = out / "soundcard.json"
    config.write_text(json.dumps({"cards": [{"name": CARD_NAME, "playback_channels": CHANNELS,
                                             "capture_channels": CHANNELS}]}))
    procs.start("tx", [args.bin_dir / f"pipe2inferno{exe}", "-c", str(CHANNELS), "-i", out / "to_card.raw"],
                device_env(TX_NAME, ip(0), 1))
    procs.start("soundcard", [args.bin_dir / f"inferno_soundcard{exe}", "--no-tray"],
                dict(device_env(CARD_DEVICE_NAME, ip(1), 3), INFERNO_SOUNDCARD_CONFIG=str(config)))
    host = procs.start("wasapi_host", [args.bin_dir / "examples" / f"wasapi_host{exe}", "--card", CARD_NAME,
                                       "--channels", str(CHANNELS), "--play", out / "from_card.raw",
                                       "--record", card_recording, "--seconds", "300", "--until-file", stop_file],
                       dict(os.environ))
    rx = procs.start("rx", [args.bin_dir / f"inferno2pipe{exe}", "-c", str(CHANNELS), "-o", rx_recording],
                     device_env(RX_NAME, ip(2), 2))
    start = time.time()
    if not wait_for_devices([TX_NAME, CARD_DEVICE_NAME, RX_NAME], start + DISCOVERY_TIMEOUT):
        log("❌ devices not discovered in time")
        return False
    log(f"✅ devices discovered ({time.time() - start:.0f} s)")
    if not check_clock([out / "tx.log", out / "rx.log", out / "soundcard.log"]):
        return False
    if not subscribe(TX_NAME, CARD_DEVICE_NAME) or not subscribe(CARD_DEVICE_NAME, RX_NAME):
        return False
    if not check_subscribed([CARD_DEVICE_NAME, RX_NAME]):
        return False
    log("✅ subscribed")
    time.sleep(RECORD_SECONDS)
    if procs.exited():
        log(f"❌ a process exited early: {procs.exited()}")
        return False
    # the recorder first: once the application stops, the card transmits silence
    rx.kill()
    rx.wait()
    stop_file.write_text("stop")
    try:
        code = host.wait(timeout=30)
    except subprocess.TimeoutExpired:
        log("❌ the WASAPI application didn't stop")
        return False
    log((out / "wasapi_host.log").read_text(errors="replace"))
    if code != 0:
        log(f"❌ the WASAPI application failed ({code})")
        return False
    ok = check_recording(to_card, card_recording, "Pipe2inferno -> sound card microphone")
    return check_recording(from_card, rx_recording, "sound card speakers -> Inferno2pipe") and ok


def main():
    # the Windows console's code page can't print ✅/❌
    sys.stdout.reconfigure(encoding="utf-8")
    parser = argparse.ArgumentParser()
    parser.add_argument("--scenario", choices=["pipe", "asio", "soundcard", "pipe-leader"], default="pipe")
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--ips", required=True, help="comma-separated addresses of this host for the devices")
    parser.add_argument("--out-dir", type=Path, required=True)
    args = parser.parse_args()
    args.ips = args.ips.split(",")
    args.out_dir.mkdir(parents=True, exist_ok=True)
    exe = ".exe" if sys.platform == "win32" else ""

    procs = Processes(args.out_dir)
    if args.scenario == "pipe-leader":
        DEVICE_ENV["INFERNO_CLOCK_LEADER"] = "auto"
    else:
        procs.start("leader", [args.bin_dir / "examples" / f"ptpv1_test_leader{exe}", args.ips[0]],
                    dict(os.environ, RUST_LOG="info"))
    ok = False
    try:
        scenarios = {"pipe": pipe_scenario, "asio": asio_scenario, "soundcard": soundcard_scenario,
                     "pipe-leader": pipe_scenario}
        ok = scenarios[args.scenario](args, procs, exe)
        return 0 if ok else 1
    finally:
        procs.stop(failed=not ok)


if __name__ == "__main__":
    sys.exit(main())
