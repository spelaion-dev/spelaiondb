#!/usr/bin/env python3
"""Check Docker Compose's resolved JSON, including future services, before CI starts it."""

import json
import sys

MIB = 1024 * 1024
MAX_SERVICES = 11776 * MIB
# GitHub specifies RAM in GB; actual 16 GB runners have different kernel reservations.
# Keep four decimal GB for the host and tests, checked against the daemon's real RAM.
HOST_RESERVE = 4_000_000_000


def check(config, host_memory):
    services = config["services"]
    if not services:
        raise ValueError("No test services were resolved")
    total = 0
    for name, service in services.items():
        limit = service.get("mem_limit")
        # Compose versions emit the normalized byte count either as JSON text or a number.
        if isinstance(limit, str) and limit.isascii() and limit.isdecimal():
            limit = int(limit)
        if not isinstance(limit, int) or isinstance(limit, bool) or limit <= 0:
            raise ValueError(f"{name}: a positive mem_limit is required")
        total += limit
    if total > MAX_SERVICES:
        raise ValueError(f"Services need {total / MIB:g} MiB, above the 11.5 GiB budget")
    if total + HOST_RESERVE > host_memory:
        raise ValueError(
            f"Services leave {(host_memory - total) / MIB:g} MiB on the Docker host; "
            "at least 4 GB is required for kind, Rust and the host"
        )
    return total


if __name__ == "__main__":
    # Docker reports the daemon's real RAM, not the client machine's or a label's promise.
    try:
        host_memory = int(sys.argv[1])
        total = check(json.load(sys.stdin), host_memory)
    except (ValueError, KeyError, IndexError) as error:
        sys.exit(str(error))
    print(
        f"Test service limits: {total / MIB:g} MiB; "
        f"Docker host RAM: {host_memory / MIB:g} MiB; "
        f"host reserve: {(host_memory - total) / MIB:g} MiB "
        f"(minimum {HOST_RESERVE / MIB:g} MiB = 4 GB)"
    )
