#!/usr/bin/env python3
"""Check Docker Compose's resolved JSON, including future services, before CI starts it."""

import json
import sys

MIB = 1024 * 1024
MAX_SERVICES = 11776 * MIB
# GitHub specifies RAM in GB; actual 16 GB runners have different kernel reservations.
# Keep four decimal GB for the host and tests, checked against the daemon's real RAM.
HOST_RESERVE = 4_000_000_000


def check(config, host_memory, selected=None):
    services = config["services"]
    if not services:
        raise ValueError("No test services were resolved")
    if selected is None:
        active = {name for name, service in services.items() if not service.get("profiles")}
    else:
        active = set(selected)
        unknown = active.difference(services)
        if unknown:
            raise ValueError(f"Unknown test services: {', '.join(sorted(unknown))}")
    if not active:
        raise ValueError("No active test services were selected")
    total = 0
    for name, service in services.items():
        limit = service.get("mem_limit")
        # Compose versions emit the normalized byte count either as JSON text or a number.
        if isinstance(limit, str) and limit.isascii() and limit.isdecimal():
            limit = int(limit)
        if not isinstance(limit, int) or isinstance(limit, bool) or limit <= 0:
            raise ValueError(f"{name}: a positive mem_limit is required")
        # Even inactive/profiled services must declare a limit. Only the running phase
        # contributes to its budget; the workflow stops the first phase before the next.
        if name in active:
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
        total = check(json.load(sys.stdin), host_memory, sys.argv[2:] or None)
    except (ValueError, KeyError, IndexError) as error:
        sys.exit(str(error))
    print(
        f"Test service limits: {total / MIB:g} MiB; "
        f"Docker host RAM: {host_memory / MIB:g} MiB; "
        f"host reserve: {(host_memory - total) / MIB:g} MiB "
        f"(minimum {HOST_RESERVE / MIB:g} MiB = 4 GB)"
    )
