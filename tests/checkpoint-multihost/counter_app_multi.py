#!/usr/bin/env python3
"""Simple per-host counter for multi-host checkpoint/restore tests.

Each process keeps an integer counter in a local file-based database and
increments it every second.
"""

import os
import time
import uuid

DB_PATH = os.environ.get("COUNTER_DB", "counter.db")
HOST_TAG = os.environ.get("HOST_TAG", "unknown-host")
INTERVAL_SEC = 1


def load_counter() -> int:
    try:
        with open(DB_PATH, "r", encoding="utf-8") as f:
            return int(f.read().strip())
    except (FileNotFoundError, ValueError):
        return 0


def save_counter(value: int) -> None:
    with open(DB_PATH, "w", encoding="utf-8") as f:
        f.write(str(value))


def main() -> None:
    counter = load_counter()
    token = os.environ.get("COUNTER_TOKEN") or uuid.uuid4().hex
    print(
        f"[counter_app_multi:{HOST_TAG}] start counter={counter}, db={DB_PATH}, token={token}",
        flush=True,
    )

    while True:
        counter += 1
        save_counter(counter)
        print(
            f"[counter_app_multi:{HOST_TAG}] counter={counter}, token={token}",
            flush=True,
        )
        time.sleep(INTERVAL_SEC)


if __name__ == "__main__":
    main()
