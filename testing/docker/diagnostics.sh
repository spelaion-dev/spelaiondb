#!/usr/bin/env bash
# Capture this immediately when startup fails: compilation may still be running, and
# docker stats omits the container the kernel already killed. State includes OOMKilled.
set -u
compose=(docker compose -f testing/docker/compose.yaml --profile '*')
echo '::group::Test service memory and health'
free -m || true
docker stats --no-stream --all || true
"${compose[@]}" ps --all || true
while IFS= read -r container; do
  [ -n "$container" ] || continue
  docker inspect --format '{{.Name}} memory_limit={{.HostConfig.Memory}} state={{json .State}}' "$container" || true
done < <("${compose[@]}" ps --all --quiet)
"${compose[@]}" logs --no-color --tail 100 || true
echo '::endgroup::'
