"""Grants the microphone permission (TCC) to processes of a macOS CI runner, so that the end-to-end test's
Core Audio application records the sound card instead of silence. Needs sudo, and a runner whose TCC databases
can be written (GitHub's macOS runners).

Usage: sudo python3 grant_microphone.py <executable path>...

The permission is checked for the process responsible for the recording application, the runner's (its
provisioner, launched by launchd), so the paths given should include it; this script adds the runner's own
processes it finds.
"""

import sqlite3
import subprocess
import sys
import time
from pathlib import Path

SERVICE = "kTCCServiceMicrophone"


def runner_processes():
    """Executables of the runner's processes (the provisioner, Runner.Listener, ...)."""
    out = subprocess.run(["ps", "-axo", "comm="], capture_output=True, text=True).stdout
    return {line.strip() for line in out.splitlines()
            if any(name in line for name in ("provisioner", "Runner.Listener", "Runner.Worker"))}


def grant(db, clients):
    connection = sqlite3.connect(db)
    columns = [row[1] for row in connection.execute("PRAGMA table_info(access)")]
    now = int(time.time())
    # the access table's columns differ between macOS versions: set those this one has
    defaults = {
        "service": SERVICE, "client_type": 1, "auth_value": 2, "auth_reason": 4, "auth_version": 1,
        "csreq": None, "policy_id": None, "indirect_object_identifier_type": 0,
        "indirect_object_identifier": "UNUSED", "indirect_object_code_identity": None, "flags": 0,
        "last_modified": now, "pid": None, "pid_version": None, "boot_uuid": "UNUSED", "last_reminded": now,
        "allowed": 1, "prompt_count": 1,
    }
    for client in clients:
        row = {c: defaults.get(c) for c in columns}
        row["client"] = client
        names = ", ".join(row)
        marks = ", ".join("?" for _ in row)
        connection.execute(f"INSERT OR REPLACE INTO access ({names}) VALUES ({marks})", list(row.values()))
        print(f"{db}: {SERVICE} granted to {client}")
    connection.commit()
    connection.close()


def main():
    clients = set(sys.argv[1:]) | runner_processes()
    databases = [Path("/Library/Application Support/com.apple.TCC/TCC.db")]
    databases += Path("/Users").glob("*/Library/Application Support/com.apple.TCC/TCC.db")
    granted = 0
    for db in databases:
        try:
            grant(db, sorted(clients))
            granted += 1
        except sqlite3.Error as e:
            print(f"{db}: {e}")
    sys.exit(0 if granted else 1)


if __name__ == "__main__":
    main()
