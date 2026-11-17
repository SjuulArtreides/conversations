#!/usr/bin/env bash
# Reproducible redeploy of arrakis-conversations into the local kind cluster.
#
# WHY THIS EXISTS: a mutable `:dev` tag + imagePullPolicy: IfNotPresent silently
# serves STALE code — the kubelet keeps the cached image and a `kubectl rollout`
# with the same tag is a no-op at the node level. This script instead builds a
# UNIQUE tag (`dev-<git-sha>`), loads it into kind, and `set image`s the
# Deployment onto that tag, so every redeploy provably runs the code you built.
#
# KIND IS DEVSHELL-ONLY. `kind` is NOT on the base PATH — it lives only in the
# nix flake devshell (`docker` and `kubectl` ARE on the base PATH). This script
# auto-detects: if `kind` is already on PATH it uses it directly; otherwise it
# runs `kind` via `direnv exec .` (the repo's `.envrc` is `use flake`). You can
# also just run the whole script inside the devshell:
#
#     direnv exec . bash scripts/redeploy-kind.sh
#
# Run from anywhere; it cd's to the repo root itself. Requires: docker, kubectl
# (base PATH) and kind (devshell). A built kind cluster named
# `arrakis-conversations` must already exist — create it once with
# `./deploy/kind-up.sh` (or `direnv exec . ./deploy/kind-up.sh`).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

CLUSTER="arrakis-conversations"
NAMESPACE="arrakis-conversations"
DEPLOY="conversations"
IMAGE="arrakis-conversations"
APP_URL="http://localhost:8080"

# Unique, content-addressed-ish tag so the node can never serve a stale layer.
SHA="$(git rev-parse --short HEAD)"
TAG="dev-${SHA}"
# `-dirty` marker when there are uncommitted changes, so a rebuild of a dirty
# tree gets a fresh tag instead of colliding with the last clean build.
if ! git diff --quiet || ! git diff --cached --quiet; then
  TAG="${TAG}-dirty-$(date +%s)"
fi
UNIQUE_IMAGE="${IMAGE}:${TAG}"

# --- Resolve how to invoke `kind` (base PATH vs devshell) -------------------
if command -v kind >/dev/null 2>&1; then
  kind_cmd() { kind "$@"; }
elif command -v direnv >/dev/null 2>&1; then
  echo "==> kind not on PATH; invoking it through the flake devshell (direnv exec .)"
  kind_cmd() { direnv exec . kind "$@"; }
else
  echo "ERROR: 'kind' is not on PATH and 'direnv' is unavailable to reach the" >&2
  echo "       flake devshell. Run this inside the devshell instead:" >&2
  echo "         direnv exec . bash scripts/redeploy-kind.sh" >&2
  exit 1
fi

for tool in docker kubectl; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "ERROR: required tool '$tool' not found on PATH" >&2
    exit 1
  fi
done

# --- Preflight: the cluster must already exist ------------------------------
if ! kind_cmd get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
  echo "ERROR: kind cluster '$CLUSTER' does not exist." >&2
  echo "       Create it first: direnv exec . ./deploy/kind-up.sh" >&2
  exit 1
fi

echo "==> [1/6] Building image ${IMAGE}:dev (-f deploy/Dockerfile)"
docker build -f deploy/Dockerfile -t "${IMAGE}:dev" .

echo "==> [2/6] Retagging to unique tag ${UNIQUE_IMAGE}"
docker tag "${IMAGE}:dev" "$UNIQUE_IMAGE"

echo "==> [3/6] Loading ${UNIQUE_IMAGE} into kind cluster ${CLUSTER}"
# Load BEFORE `set image`: with IfNotPresent, a not-yet-loaded tag makes the
# kubelet try docker.io/library/... -> ImagePullBackOff.
kind_cmd load docker-image "$UNIQUE_IMAGE" --name "$CLUSTER"

echo "==> [4/6] Applying kustomize overlay deploy/kind (NodePort -> localhost:8080)"
# MUST be deploy/kind, NOT deploy/k8s: the base Service is ClusterIP and would
# overwrite the NodePort, killing localhost:8080.
kubectl apply -k deploy/kind

echo "==> [5/6] Pointing deploy/${DEPLOY} at ${UNIQUE_IMAGE}"
kubectl -n "$NAMESPACE" set image "deploy/${DEPLOY}" "${DEPLOY}=${UNIQUE_IMAGE}"

echo "==> [6/6] Waiting for rollout (timeout 180s)"
kubectl -n "$NAMESPACE" rollout status "deploy/${DEPLOY}" --timeout=180s

# --- Smoke check ------------------------------------------------------------
echo "==> Smoke check: ${APP_URL}/healthz"
if ! curl -fsS "${APP_URL}/healthz" >/dev/null; then
  echo "ERROR: /healthz did not respond OK at ${APP_URL}" >&2
  echo "       Is the kind cluster's port mapping up (NodePort 30080 -> 8080)?" >&2
  exit 1
fi

# /api/llm/status is a real JSON API route. A STALE image that predates it (or
# any build where the SPA shadows the route) falls through to the static
# index.html and returns text/html. Asserting application/json here catches a
# stale-image deploy that a plain /healthz check would miss.
echo "==> Smoke check: ${APP_URL}/api/llm/status returns JSON (stale-image guard)"
CTYPE="$(curl -fsS -o /dev/null -w '%{content_type}' "${APP_URL}/api/llm/status")"
case "$CTYPE" in
  application/json*)
    echo "    OK: content-type=${CTYPE}"
    ;;
  *)
    echo "ERROR: /api/llm/status returned content-type '${CTYPE}', expected application/json." >&2
    echo "       This usually means a STALE image is running (the route is missing" >&2
    echo "       and the SPA index.html is served instead). The new tag may not" >&2
    echo "       have loaded — re-run, and confirm the loaded tag matches ${TAG}." >&2
    exit 1
    ;;
esac

echo
echo "==> Done. ${DEPLOY} is running ${UNIQUE_IMAGE}"
echo "    App:  ${APP_URL}"
echo "    Tag:  ${TAG}"
