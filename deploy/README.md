# Deploying arrakis-conversations

Self-hosting on Kubernetes. The image is a single multi-stage build that bundles
the React SPA (served as static) and the Rust server (JSON API + static serving).

## ⚠️ Security & exposure — read first

The server has two postures, controlled by `AUTH_ENABLED`:

- **`AUTH_ENABLED=true` (default): login required.** API data endpoints return
  401 without a session; sharing (`organization` vs per-email) is enforced. With
  no other configuration, users log in with email and password
  (`PASSWORD_AUTH_ENABLED` defaults to `true`). Google SSO is optional; see
  below.
- **`AUTH_ENABLED=false`: NO authentication and NO authorization.** Anyone who
  can reach the port can read, change, and delete every session and upload new
  ones. The per-session sharing model is stored but **not enforced**. This is an
  explicit opt-out for local development and tests on a trusted machine. Never
  set it on a shared or reachable deployment.

At startup the server checks the auth configuration and exits with an error,
instead of serving, when:

- no login method is usable (`PASSWORD_AUTH_ENABLED=false` and Google SSO not
  configured), or
- Google SSO is partially configured (some, but not all, of
  `GOOGLE_CLIENT_ID`, `GOOGLE_CLIENT_SECRET`, `OIDC_REDIRECT_URL`).

What an operator configures:

- **Email + password only:** nothing beyond the default. Because registration
  has no email verification by default, create the administrator accounts first, then list
  them in `INITIAL_ADMIN_EMAILS` (comma-separated). Those addresses cannot
  self-register afterwards and cannot be revoked in the app.
- **Google SSO:** set all of `GOOGLE_CLIENT_ID`, `GOOGLE_CLIENT_SECRET`, and
  `OIDC_REDIRECT_URL` (optionally `ALLOWED_HOSTED_DOMAIN`). Set
  `PASSWORD_AUTH_ENABLED=false` to make Google the only login method.

**Upgrading from a release where auth was off by default:** a deployment that
never set `AUTH_ENABLED` now requires login after the upgrade. Set the variables
above before you roll out. Keeping `AUTH_ENABLED=false` is possible but leaves
every session open to anyone who can reach the server.

**Single-replica constraint:** keep `replicas: 1`. Live connections and host
relay state are held in process memory. Selecting Postgres does not make the
server safe to run with multiple replicas.

## Image architecture

The Dockerfile supports ARM64 and x86-64. Replace `<registry>` and `vX.Y.Z`
with your registry and version, then publish both under one image tag:

```sh
docker buildx build --platform linux/amd64,linux/arm64 \
  -f deploy/Dockerfile -t <registry>/arrakis-conversations:vX.Y.Z --push .
```

Docker selects the matching image when users pull that tag. The build needs
native builders for both architectures or configured emulation, such as Docker
Desktop provides. For a single local image, choose `--platform linux/arm64` or
`--platform linux/amd64` and use `--load` instead of `--push`.

## Local kind cluster (one command)

```sh
./deploy/kind-up.sh
```

This builds `arrakis-conversations:dev`, creates a kind cluster, loads the image,
applies `deploy/kind` (base manifests + a NodePort overlay), and waits for the
rollout. The app is then at <http://localhost:8080>.

```sh
curl http://localhost:8080/healthz
conversations --server http://localhost:8080 upload --all
```

Tear down: `kind delete cluster --name arrakis-conversations`.

## Manifests (`deploy/k8s`, kustomize base)

| File | Purpose |
|------|---------|
| `namespace.yaml` | `arrakis-conversations` namespace |
| `configmap.yaml` | non-secret config (bind addr, `DATABASE_URL`, `AUTH_ENABLED`, …) |
| `secret.example.yaml` | template for the Google SSO secret (copy to `secret.yaml`) |
| `pvc.yaml` | 2Gi RWO volume for the SQLite DB at `/data` |
| `deployment.yaml` | the server Deployment (replicas=1, Recreate, probes, non-root) |
| `service.yaml` | ClusterIP service |
| `ingress.yaml` | optional Ingress (not in the kustomization by default) |

Apply the base directly on a real cluster with `kubectl apply -k deploy/k8s`
(set a real image via the `images:` field or `kustomize edit set image`).

## Storage backends (swappable)

- **SQLite (default)** — `DATABASE_URL=sqlite:///data/conversations.db?mode=rwc`
  on the PVC. Single-writer, so keep `replicas: 1` (the Deployment uses the
  `Recreate` strategy to avoid two pods mounting the RWO volume at once).
- **Postgres** — set `DATABASE_URL=postgres://user:pass@host:5432/db`. The server
  selects the Postgres backend by URL scheme. Keep `replicas: 1` for live
  connection and host relay state. Provision Postgres separately and put the
  URL in a Secret.

## SMTP signup verification (optional)

Set all three keys in a Kubernetes Secret named `conversations-signup-smtp`
in the `arrakis-conversations` namespace, then restart the deployment. Keep
the secret values in your secret manager.

| Secret key | Value |
|------|---------|
| `SIGNUP_SMTP_URL` | Authenticated `smtps://` URL. Percent-encode the username and password. For TransIP, the relay is `smtp.transip.email:465`. |
| `SIGNUP_EMAIL_FROM` | Sender mailbox address, e.g. `conversations@kubemaster.com`. |
| `SIGNUP_PUBLIC_URL` | Public HTTPS origin, e.g. `https://conversations.kubemaster.com`. |

The sign-up form sends an email link. The recipient follows that link to
choose a password and create their account. Links expire after 30 minutes
and work once. A recipient can request another link after one minute, which
replaces their previous link. The server limits signup mail to 60 attempts
per hour. Existing password accounts and active sessions keep their access.

## Google SSO (optional)

Auth is on by default with email+password login; Google SSO is not required.
To add it:

1. Create a Google OAuth client (Web application) with redirect URI
   `https://<your-host>/auth/callback`.
2. `cp deploy/k8s/secret.example.yaml deploy/k8s/secret.yaml`, fill in the
   Google client id/secret, then `kubectl apply -f` it. (No session secret is
   needed — the cookie is an unsigned opaque server-side id.)
3. Set the correct `OIDC_REDIRECT_URL` in the ConfigMap and roll the
   Deployment.

`secret.yaml` is gitignored — never commit real secrets. `GOOGLE_CLIENT_SECRET`
flows in as a plain Kubernetes Secret. Use your deployment's secret manager
to populate it. With `OIDC_REDIRECT_URL` set but the secret missing, the
server fails fast at startup with a clear error (it does not silently run with
broken auth).

### Other config knobs (ConfigMap)

| Env | Default | Purpose |
|-----|---------|---------|
| `COOKIE_SECURE` | `true` when auth on + https redirect, else `false` | Sets the `Secure` flag on the session cookie. Leave unset for local http; set `"true"` behind TLS. |
| `MAX_UPLOAD_BYTES` | `33554432` (32 MiB) | Caps session-upload and live-event request bodies (matches the ingress `proxy-body-size`). |
| `ALLOWED_HOSTED_DOMAIN` | unset | Restrict login to one email domain (matched against the verified email's domain). |
