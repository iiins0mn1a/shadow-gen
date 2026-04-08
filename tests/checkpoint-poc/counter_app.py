#!/usr/bin/env python3
"""Simple counter application for Shadow checkpoint/restore POC.

Every second, increments a counter and writes the current value to a local
file-based "database" (counter.db). This allows the external orchestrator to
verify that restoring a checkpoint rewinds the application state.
"""
import time
import os
import sys

DB_PATH = os.environ.get("COUNTER_DB", "/tmp/counter.db")
INTERVAL = 1  # seconds between increments

def load_counter():
    try:
        with open(DB_PATH, "r") as f:
            return int(f.read().strip())
    except (FileNotFoundError, ValueError):
        return 0

def save_counter(value):
    with open(DB_PATH, "w") as f:
        f.write(str(value))
    f.close()

def main():
    counter = load_counter()
    print(f"[counter_app] Starting with counter={counter}, db={DB_PATH}", flush=True)

    while True:
        counter += 1
        save_counter(counter)
        print(f"[counter_app] counter={counter}", flush=True)
        time.sleep(INTERVAL)

if __name__ == "__main__":
    main()
