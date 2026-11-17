# Operating the local kind deploy

Operator runbook for redeploying `arrakis-conversations` into the local kind
cluster without losing hours to silent stale-image deploys. For the manifest
reference, image architecture, auth postures, and storage backends see
[`README.md`](./README.md).

## TL;DR — redeploy after a code change

```sh
# kind lives only in the flake devshell, so run the script through it:
direnv exec . bash scripts/redeploy-kind.sh
```

That builds the image, tags it uniquely, loads it into kind, applies the
`deploy/kind` overlay, rolls the Deployment onto the new tag, and smoke-tests
the app at <http://localhost:8080>. First-time setup (creating the cluster) is
`direnv exec . ./deploy/kind-up.sh`.

Constants the script assumes: cluster `arrakis-conversations`, namespace
`arrakis-conversations`, Deployment `conversations`, app at
`http://localhost:8080`.

## What the script does (and why each step matters)

1. **Build** `arrakis-conversations:dev` via `docker build -f deploy/Dockerfile`.
2. **Retag** to a unique `dev-<git-sha>` tag (`-dirty-<ts>` suffix on a dirty
   tree).
3. **`kind load`** that unique tag into the cluster — through the devshell.
4. **`kubectl apply -k deploy/kind`** so the NodePort overlay is present.
5. **`kubectl set image`** the Deployment onto the unique tag.
6. **`rollout status --timeout=180s`**, then smoke checks.

## Gotchas — read these before you debug a "my change didn't show up"

### 1. Always `kubectl apply -k deploy/kind`, never `deploy/k8s`

The `deploy/k8s` base Service is `ClusterIP`. The `deploy/kind` kustomize
overlay patches it to a `NodePort` (30080), which the kind cluster maps to
`hostPort 8080`. Applying the **base** (`-k deploy/k8s`) reverts the Service to
ClusterIP and **breaks `http://localhost:8080`**. The redeploy script always
uses `deploy/kind`; do the same for any manual `kubectl apply`.

### 2. `kind` is only in the flake devshell

`kind` is **not** on the base PATH — it ships only in the nix flake devshell
(`.envrc` is `use flake`). A bare `kind load ...` fails with
`command not found`. (`docker` and `kubectl` *are* on the base PATH.) Either
run the whole script through the devshell:

```sh
direnv exec . bash scripts/redeploy-kind.sh
```

…or rely on the script's auto-detection: if `kind` is missing from PATH it
re-invokes `kind` via `direnv exec .` itself.

### 3. Unique tags beat `:dev` + `IfNotPresent` (the silent-stale trap)

A mutable `:dev` tag with `imagePullPolicy: IfNotPresent` is **unreliable**: the
kubelet keeps the cached `:dev` image, so a rebuilt-but-same-tag image is never
picked up — the node serves stale code and nothing errors. The robust pattern
(what the script does) is a **unique tag** `dev-$(git rev-parse --short HEAD)`:

```sh
docker build -f deploy/Dockerfile -t arrakis-conversations:dev .
docker tag  arrakis-conversations:dev arrakis-conversations:dev-<sha>
direnv exec . kind load docker-image arrakis-conversations:dev-<sha> --name arrakis-conversations
kubectl apply -k deploy/kind
kubectl -n arrakis-conversations set image deploy/conversations conversations=arrakis-conversations:dev-<sha>
kubectl -n arrakis-conversations rollout status deploy/conversations --timeout=180s
```

**Order matters:** `kind load` the tag **before** `set image`. With
`IfNotPresent`, pointing the Deployment at a tag the node doesn't have yet makes
the kubelet try to pull `docker.io/library/arrakis-conversations:...` →
`ImagePullBackOff`.

### 4. The stale-image smoke check

After the rollout the script asserts that `GET /api/llm/status` returns
`application/json`. That endpoint is a real JSON API route; a stale image
lacking it falls through to the SPA and returns `text/html`. So a green
`/healthz` plus a JSON `/api/llm/status` is the signal that the **new** code is
actually live — not just that *some* pod is up.

## One-time: the `openrouter` secret (optional LLM features)

The server reads `OPENROUTER_API_KEY` from a Secret named `openrouter` (key
`OPENROUTER_API_KEY`). It is **optional** — the pod starts fine without it and
LLM features (session summary + chat) simply stay off. To enable them:

```sh
kubectl create secret generic openrouter \
  -n arrakis-conversations \
  --from-literal=OPENROUTER_API_KEY=sk-or-v1-...
```

Then roll the Deployment so the pod re-reads the env (`kubectl -n
arrakis-conversations rollout restart deploy/conversations`, or just run the
redeploy script). There is a template at
[`k8s/secret.example.yaml`](./k8s/secret.example.yaml) (also covers the optional
Google SSO secret). `secret.yaml` is gitignored — never commit real secrets.

## Tear down

```sh
direnv exec . kind delete cluster --name arrakis-conversations
```
