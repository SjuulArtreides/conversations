import "fake-indexeddb/auto";
// @ts-expect-error -- node types are not part of the app tsconfig; this is a dev-only generator
import { writeFileSync } from "node:fs";
import { describe, it } from "vitest";
import {
  loadBrowserKey,
  operationPayload,
  signPayload,
  toBase64,
} from "./hostAuth";

declare const process: { env: Record<string, string | undefined> };

// Regenerates `fixtures/app_signed_pr_settings_ops.json`: the PR and settings
// operations as the SPA's own production modules sign them (hostAuth.ts:
// loadBrowserKey, operationPayload, signPayload), for the native host to verify
// (`operations_signed_by_the_apps_own_code_for_pr_and_settings_are_verified_by_the_real_host`).
// The key is a real WebCrypto P-256 key; this runs under Node's WebCrypto in
// vitest, NOT in a browser, and says so in the fixture. Run with REGEN_APP_OPS=1.
const HOST = "_oEsEvOrTOasXbaaw1L5BssbEe9D-zPiUu9_9VImOIk";
const DLG = "00112233445566778899aabbccddeeff";
const SESSION = "3f1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b11";

describe.skipIf(!process.env.REGEN_APP_OPS)(
  "regenerate signed PR/settings ops",
  () => {
    it("writes the fixture", async () => {
      const key = await loadBrowserKey();
      const sign = async (
        op: Parameters<typeof operationPayload>[0]["op"],
        session: string,
        args: Record<string, unknown>,
      ) =>
        signPayload(
          key,
          operationPayload({
            hostId: HOST,
            key,
            dlg: DLG,
            op,
            session,
            args,
            now: 1_800_000_000,
          }),
        );
      const launch = {
        claude_skip_permissions: true,
        opencode_auto: false,
        codex_sandbox_mode: "read-only",
        codex_approval_policy: null,
        default_launcher: "adk",
        adk_statusline: true,
      };
      const frames = {
        pr_comment: await sign("pr.comment", SESSION, {
          owner: "octo",
          repo: "repo",
          number: 7,
          body: "Looks good.\n-- thanks `x` $(y)",
          in_reply_to: 99,
        }),
        pr_sync: await sign("pr.sync", SESSION, {
          owner: "octo",
          repo: "repo",
          number: 7,
        }),
        settings_get: await sign("settings.get", "host", {}),
        settings_update: await sign("settings.update", "host", { launch }),
      };
      writeFileSync(
        "src/lib/fixtures/app_signed_pr_settings_ops.json",
        JSON.stringify(
          {
            _comment:
              "Produced by the SPA's production modules (hostAuth.ts) under vitest with Node WebCrypto, NOT a browser run. Host audience = fixed test host (seed [7;32]); delegation 00112233445566778899aabbccddeeff; iat 1800000000, exp +60s.",
            pub: toBase64(key.publicRaw),
            kid: key.kid,
            host: HOST,
            dlg: DLG,
            session: SESSION,
            launch,
            frames,
          },
          null,
          2,
        ) + "\n",
      );
    });
  },
);
