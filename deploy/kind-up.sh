#!/usr/bin/env bash
# Build the image, create a kind cluster, load the image, and deploy.
# Verifies the app comes up healthy. Idempotent-ish: re-run safe.
#
# Requires: docker, kind, kubectl (all in the nix flake devshell).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CLUSTER="arrakis-conversations"
IMAGE="arrakis-conversations:dev"

cd "$REPO_ROOT"

echo "==> Building image $IMAGE"
docker build -f deploy/Dockerfile -t "$IMAGE" .

if ! kind get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
  echo "==> Creating kind cluster $CLUSTER"
  kind create cluster --config deploy/kind/kind-config.yaml
else
  echo "==> kind cluster $CLUSTER already exists"
fi

echo "==> Loading image into kind"
kind load docker-image "$IMAGE" --name "$CLUSTER"

echo "==> Applying manifests"
kubectl apply -k deploy/kind

echo "==> Waiting for rollout"
kubectl -n arrakis-conversations rollout status deploy/conversations --timeout=120s

echo
echo "App should be reachable at http://localhost:8080"
echo "  health:  curl http://localhost:8080/healthz"
echo "  upload:  arrakis-conversations --server http://localhost:8080 upload --all"
echo "Tear down with: kind delete cluster --name $CLUSTER"
