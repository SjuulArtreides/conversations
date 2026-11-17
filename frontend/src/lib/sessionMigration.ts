import { hostControl } from "./hostControl";
import { fromBase64Url } from "./hostAuth";
import type { MigrationManifest } from "./privateChannel";

export class MigrationResumeError extends Error {
  constructor(
    readonly destination: string,
    cause: unknown,
  ) {
    super(
      `The session was copied, but could not resume: ${cause instanceof Error ? cause.message : String(cause)}`,
    );
  }
}

export class MigrationCopyUncertainError extends Error {
  constructor(readonly destination: string) {
    super(
      "The destination did not confirm the final copy. A copy may exist there. Check the destination before migrating again.",
    );
  }
}

/** One snapshot, one destination identity. Never stops or modifies the source. */
export async function migrateSession(input: {
  sourceHost: string;
  sourceSession: string;
  destinationHost: string;
  cwd: string;
  onProgress?: (copied: number, total: number) => void;
}): Promise<string> {
  if (input.sourceHost === input.destinationHost)
    throw new Error("Choose a different destination host.");
  if (!input.cwd.trim()) throw new Error("Choose a destination folder.");
  const transfer = crypto.randomUUID();
  const destination = crypto.randomUUID();
  let offset = 0;
  let manifest: MigrationManifest | undefined;
  const read = (offset: number) =>
    hostControl
      .exportSessionChunk(
        input.sourceHost,
        input.sourceSession,
        transfer,
        offset,
      )
      // Observe read failures even if the overlapping destination write fails first.
      .then(
        (chunk) => ({ chunk }),
        (error: unknown) => ({ error }),
      );
  let pendingRead = read(offset);
  for (;;) {
    const result = await pendingRead;
    if ("error" in result) throw result.error;
    const { chunk } = result;
    if (manifest && JSON.stringify(manifest) !== JSON.stringify(chunk.manifest))
      throw new Error("The source snapshot changed. Start migration again.");
    manifest = chunk.manifest;
    const size = fromBase64Url(chunk.b64).length;
    const end = offset + size;
    if (
      !size ||
      size > 6 * 1024 ||
      end > manifest.size ||
      (chunk.next !== null
        ? chunk.next !== end || end >= manifest.size
        : end !== manifest.size)
    )
      throw new Error("The host returned an incomplete migration snapshot.");
    // Read only one chunk ahead; destination writes remain strictly ordered.
    if (chunk.next !== null) pendingRead = read(chunk.next);
    let copied: boolean;
    try {
      copied = await hostControl.importSessionChunk(input.destinationHost, {
        transfer,
        destination,
        cwd: input.cwd.trim(),
        manifest,
        offset,
        b64: chunk.b64,
      });
    } catch (error) {
      if (chunk.next === null)
        throw new MigrationCopyUncertainError(destination);
      throw error;
    }
    if (copied !== (chunk.next === null))
      throw new Error("The destination did not confirm the complete copy.");
    input.onProgress?.(end, manifest.size);
    if (copied) break;
    offset = end;
  }
  try {
    await hostControl.reopenSession(destination, input.destinationHost);
  } catch (error) {
    throw new MigrationResumeError(destination, error);
  }
  return destination;
}
