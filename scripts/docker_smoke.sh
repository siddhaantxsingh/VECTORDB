#!/usr/bin/env bash
# Smoke test of the Docker image: build (unless SKIP_BUILD=1), run, create a
# collection, upsert, search, restart the container on the same volume, and
# check the data survived. Usage: scripts/docker_smoke.sh
set -euo pipefail
cd "$(dirname "$0")/.."
IMG=${IMG:-vectordb:smoke}
NAME=vdb-smoke-$$
VOL=vdb-smoke-$$
PORT=${PORT:-16333}
[ "${SKIP_BUILD:-0}" = 1 ] || docker build -t "$IMG" .
cleanup() { docker rm -f "$NAME" >/dev/null 2>&1 || true; docker volume rm "$VOL" >/dev/null 2>&1 || true; }
trap cleanup EXIT
start() {
  docker run -d --name "$NAME" -p 127.0.0.1:$PORT:6333 -v "$VOL":/data "$IMG" >/dev/null
  for _ in $(seq 50); do curl -sf "localhost:$PORT/healthz" >/dev/null && return; sleep 0.2; done
  docker logs "$NAME"; echo "FAIL: not healthy"; exit 1
}
H='content-type: application/json'
start
echo "== image size: $(docker image inspect "$IMG" --format '{{.Size}}') bytes"
echo "== user in container: $(docker exec "$NAME" id)"
curl -sf -X PUT "localhost:$PORT/collections/docs" -H "$H" \
  -d '{"dim": 3, "metric": "cosine", "m": 16, "ef_construction": 200, "seed": 42}'; echo
curl -sf -X PUT "localhost:$PORT/collections/docs/points" -H "$H" \
  -d '[{"id": 1, "vector": [1, 0, 0], "metadata": {"lang": "en"}}, {"id": 2, "vector": [0, 1, 0], "metadata": {"lang": "de"}}]'; echo
R1=$(curl -sf -X POST "localhost:$PORT/collections/docs/search" -H "$H" -d '{"vector": [0.9, 0.1, 0], "k": 1, "filter": {"eq": ["lang", "en"]}}')
echo "search: $R1"
echo "$R1" | grep -q '"id":1' || { echo "FAIL: search"; exit 1; }
echo "== restart (docker stop sends SIGTERM)"
docker stop "$NAME" >/dev/null; docker rm "$NAME" >/dev/null
start
R2=$(curl -sf "localhost:$PORT/collections/docs/points/2")
echo "after restart, point 2: $R2"
echo "$R2" | grep -q '"lang":"de"' || { echo "FAIL: data lost across restart"; exit 1; }
curl -sf "localhost:$PORT/metrics" | grep -m3 -i vectordb || true
echo "PASS: docker smoke test"
