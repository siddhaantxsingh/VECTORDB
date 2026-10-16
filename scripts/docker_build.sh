#!/usr/bin/env bash
# Build the image; forwards a proxy CA bundle as a build secret if present.
#   scripts/docker_build.sh [TAG]
set -euo pipefail
cd "$(dirname "$0")/.."
TAG=${1:-vectordb:latest}
CA=${PROXY_CA:-/root/.ccr/ca-bundle.crt}
args=(--network host -t "$TAG")
[ -s "$CA" ] && args+=(--secret id=ca,src="$CA")
DOCKER_BUILDKIT=1 exec docker build "${args[@]}" .
