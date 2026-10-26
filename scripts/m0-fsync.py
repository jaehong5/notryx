#!/usr/bin/env python3
import json
import os
from pathlib import Path
import tempfile
import time


with tempfile.TemporaryDirectory(prefix="notryx-m0-fsync-") as directory:
    rows = []
    with (Path(directory) / "probe").open("w+b", buffering=0) as file:
        for index in range(12):
            before = time.perf_counter_ns()
            file.seek(0)
            file.write(bytes([index]) * 4096)
            written = time.perf_counter_ns()
            os.fsync(file.fileno())
            synced = time.perf_counter_ns()
            rows.append({"write_ms": (written-before)/1e6, "fsync_ms": (synced-written)/1e6})
    print(json.dumps({"scope": "4 KiB fsync diagnostic, not a power-loss test", "samples": rows}, indent=2))
