"""End-to-end test of Inferno on Windows: Pipe2inferno transmits a known signal, Inferno2pipe receives it,
routed with netaudio like the Linux Docker test (test/dockerized_trx). Exits with the test result.

Both devices run on this host, on their own IP addresses (netaudio 0.4 keys devices by IP): pass two
addresses assigned to this host. They follow the local clock, the same performance counter in every process
on the host, so they're in sync without PTP.

Usage: python run_e2e.py --bin-dir target/debug --tx-ip 10.213.77.1 --rx-ip 10.213.77.2 --out-dir e2e-out
"""

import argparse
import array
import os
import random
import subprocess
import sys
import time
from pathlib import Path

SAMPLE_RATE = 48000
CHANNELS = 2
SOURCE_SECONDS = 150
# audio to record once subscribed
RECORD_SECONDS = 20
MIN_MATCHING_SECONDS = 15
DISCOVERY_TIMEOUT = 120
LATENCY_NS = 40_000_000
TX_NAME = "wine2etx"
RX_NAME = "wine2erx"


def log(message):
    print(message, flush=True)


def write_source(path):
    """Interleaved native-endian s32: pseudo-random 24-bit values (Dante carries 24 bits), so that any
    window of the source is unique and a recording can be aligned to it exactly."""
    count = SOURCE_SECONDS * SAMPLE_RATE * CHANNELS
    random_bytes = random.Random(1234).randbytes(3 * count)
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
        "RUST_LOG": env.get("RUST_LOG", "info"),
    })
    return env


def netaudio(*args, timeout=90):
    result = subprocess.run(["netaudio", *args], capture_output=True, text=True, timeout=timeout)
    return result.returncode, result.stdout + result.stderr


def wait_for_devices(deadline):
    while True:
        code, out = netaudio("device", "list")
        found = [name for name in (TX_NAME, RX_NAME) if name in out]
        log(f"netaudio device list (exit {code}): found {found}")
        if len(found) == 2:
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
    first = next((i for i in range(frames) if recording[i * CHANNELS] != 0), None)
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


def main():
    # the Windows console's code page can't print ✅/❌
    sys.stdout.reconfigure(encoding="utf-8")
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--tx-ip", required=True)
    parser.add_argument("--rx-ip", required=True)
    parser.add_argument("--out-dir", type=Path, required=True)
    args = parser.parse_args()
    out = args.out_dir
    out.mkdir(parents=True, exist_ok=True)
    exe = ".exe" if sys.platform == "win32" else ""

    source_path = out / "source.raw"
    recording_path = out / "recording.raw"
    source = write_source(source_path)
    log(f"source: {SOURCE_SECONDS} s, {CHANNELS} channels")

    tx_log = open(out / "tx.log", "w")
    rx_log = open(out / "rx.log", "w")
    tx = subprocess.Popen(
        [args.bin_dir / f"pipe2inferno{exe}", "-c", str(CHANNELS), "-i", source_path],
        env=device_env(TX_NAME, args.tx_ip, 1), stdout=tx_log, stderr=subprocess.STDOUT)
    rx = subprocess.Popen(
        [args.bin_dir / f"inferno2pipe{exe}", "-c", str(CHANNELS), "-o", recording_path],
        env=device_env(RX_NAME, args.rx_ip, 2), stdout=rx_log, stderr=subprocess.STDOUT)
    result = 1
    try:
        start = time.time()
        if not wait_for_devices(start + DISCOVERY_TIMEOUT):
            log("❌ devices not discovered in time")
            return 1
        log(f"✅ devices discovered ({time.time() - start:.0f} s)")

        code, output = netaudio("subscription", "add", "--tx", TX_NAME, "--rx", RX_NAME)
        log(output)
        if code != 0:
            log("❌ subscription add failed")
            return 1
        code, output = netaudio("subscription", "list")
        log(output)
        subscribed = [line for line in output.splitlines() if RX_NAME in line and "Subscribed" in line]
        if len(subscribed) < CHANNELS:
            log(f"❌ expected {CHANNELS} subscribed channels")
            return 1
        log("✅ subscribed")

        time.sleep(RECORD_SECONDS)
        if tx.poll() is not None or rx.poll() is not None:
            log(f"❌ a device exited early: tx {tx.poll()}, rx {rx.poll()}")
            return 1
        rx.kill()
        rx.wait()
        matching = compare(source, recording_path)
        if matching < MIN_MATCHING_SECONDS * SAMPLE_RATE:
            log(f"❌ less than {MIN_MATCHING_SECONDS} s of the recording matches the source exactly")
            return 1
        log(f"✅ {matching / SAMPLE_RATE:.1f} s of audio received exactly as transmitted")
        result = 0
        return 0
    finally:
        for process in (tx, rx):
            if process.poll() is None:
                process.kill()
                process.wait()
        tx_log.close()
        rx_log.close()
        if result != 0:
            for name in ("tx.log", "rx.log"):
                log(f"----- {name} (last lines)")
                log("\n".join((out / name).read_text(errors="replace").splitlines()[-60:]))


if __name__ == "__main__":
    sys.exit(main())
