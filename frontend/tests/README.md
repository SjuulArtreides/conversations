# Frontend checks

Run `npm ci`, `npm run build`, `npm run lint`, and `npm test` from `frontend/`.
The regular tests are colocated with components and libraries under `src/`.

The historical standalone PTY recovery harness is not included: its required
server example is absent from the source revision used for this snapshot.
The normal CLI tests still exercise the sanitized interactive PTY fixture.

Scripts in this directory are separate integration checks, not part of Vitest.
Read each script's prerequisites before running it. Use isolated local server
and host instances; do not point a test at a shared deployment.
