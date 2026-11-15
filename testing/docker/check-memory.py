#!/usr/bin/env python3
"""Check Docker Compose's resolved JSON, including future services, before CI starts it."""

import json
import sys

MIB = 1024 * 1024
MAX_SERVICES = 11520 * MIB
HOST_RESERVE = 4 * 1024 * MIB


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
        raise ValueError(f"Services need {total / MIB:g} MiB, above the 11.25 GiB budget")
    if total + HOST_RESERVE > host_memory:
        raise ValueError("Services leave less than 4 GiB for kind, Rust and the host")
    return total


if __name__ == "__main__":
    # Docker reports the daemon's real RAM, not the client machine's or a label's promise.
    try:
        total = check(json.load(sys.stdin), int(sys.argv[1]))
    except (ValueError, KeyError, IndexError) as error:
        sys.exit(str(error))
    print(f"Test service limits: {total / MIB:g} MiB; host reserve: at least 4096 MiB")
