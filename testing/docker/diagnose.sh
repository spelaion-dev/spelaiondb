#!/usr/bin/env bash
# What CI prints when the test services did not all become healthy (SDB-345): the memory each
# container uses against its limit, the runner's memory, each service's state (with whether the
# kernel killed it for memory), and the last lines of every service that is not healthy.
cd "$(dirname "$0")"
echo "::group::docker stats"
docker stats --no-stream --format 'table {{.Name}}\t{{.MemUsage}}\t{{.MemPerc}}\t{{.CPUPerc}}'
echo "::endgroup::"
echo "::group::the runner's memory"
free -m 2>/dev/null || true
echo "::endgroup::"
echo "::group::services"
for id in $(docker compose -f compose.yaml ps -a -q); do
  docker inspect "$id" --format \
    '{{.Name}} {{.State.Status}} health={{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}} exit={{.State.ExitCode}} oom-killed={{.State.OOMKilled}} restarts={{.RestartCount}}'
done
echo "::endgroup::"
for service in $(docker compose -f compose.yaml config --services); do
  id=$(docker compose -f compose.yaml ps -a -q "$service")
  state=$(docker inspect "$id" --format '{{.State.Status}} {{if .State.Health}}{{.State.Health.Status}}{{end}}' 2>/dev/null)
  case "$state" in
    "running healthy" | "running ") continue ;;
  esac
  echo "::group::$service ($state): its last lines"
  docker compose -f compose.yaml logs --no-color --tail 60 "$service"
  echo "::endgroup::"
done
