const DATABASE_NAME = "arrakis-conversations-terminal-cache";
const DATABASE_VERSION = 1;
const STORE_NAME = "snapshots";

const MAX_ENTRIES = 8;
const MAX_ENTRY_BYTES = 8 * 1024 * 1024;
const MAX_TOTAL_BYTES = 32 * 1024 * 1024;
const MAX_DIMENSION = 10_000;

export interface TerminalSnapshot {
  id: string;
  serialized: string;
  cols: number;
  rows: number;
  byteSize: number;
  lastUsedAt: number;
}

let databasePromise: Promise<IDBDatabase | null> | null = null;
let writeQueue: Promise<void> = Promise.resolve();
let lastUsageStamp = 0;
let cacheWritesEnabled = true;

function nextUsageStamp(): number {
  lastUsageStamp = Math.max(Date.now(), lastUsageStamp + 1);
  return lastUsageStamp;
}

function openDatabase(): Promise<IDBDatabase | null> {
  if (typeof indexedDB === "undefined") return Promise.resolve(null);
  if (databasePromise) return databasePromise;

  databasePromise = new Promise((resolve) => {
    const request = indexedDB.open(DATABASE_NAME, DATABASE_VERSION);
    request.onupgradeneeded = () => {
      if (!request.result.objectStoreNames.contains(STORE_NAME)) {
        request.result.createObjectStore(STORE_NAME, { keyPath: "id" });
      }
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => resolve(null);
    request.onblocked = () => resolve(null);
  });
  return databasePromise;
}

function requestResult<T>(request: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

function transactionDone(transaction: IDBTransaction): Promise<void> {
  return new Promise((resolve, reject) => {
    transaction.oncomplete = () => resolve();
    transaction.onerror = () => reject(transaction.error);
    transaction.onabort = () => reject(transaction.error);
  });
}

function isTerminalSnapshot(value: unknown): value is TerminalSnapshot {
  if (value == null || typeof value !== "object") return false;
  const entry = value as Partial<TerminalSnapshot>;
  return (
    typeof entry.id === "string" &&
    entry.id.length > 0 &&
    typeof entry.serialized === "string" &&
    Number.isInteger(entry.cols) &&
    entry.cols! > 0 &&
    entry.cols! <= MAX_DIMENSION &&
    Number.isInteger(entry.rows) &&
    entry.rows! > 0 &&
    entry.rows! <= MAX_DIMENSION &&
    Number.isInteger(entry.byteSize) &&
    entry.byteSize! >= 0 &&
    entry.byteSize! <= MAX_ENTRY_BYTES &&
    Number.isFinite(entry.lastUsedAt) &&
    entry.lastUsedAt! >= 0
  );
}

function serializedByteSize(serialized: string): number {
  return new TextEncoder().encode(serialized).byteLength;
}

async function writeAndPrune(entry: TerminalSnapshot): Promise<void> {
  const database = await openDatabase();
  if (!database) return;

  const transaction = database.transaction(STORE_NAME, "readwrite");
  const store = transaction.objectStore(STORE_NAME);
  store.put(entry);

  const values = await requestResult(store.getAll());
  const snapshots = values
    .filter(isTerminalSnapshot)
    .sort((left, right) => right.lastUsedAt - left.lastUsedAt);

  let retainedBytes = 0;
  snapshots.forEach((snapshot, index) => {
    retainedBytes += snapshot.byteSize;
    if (index >= MAX_ENTRIES || retainedBytes > MAX_TOTAL_BYTES) {
      store.delete(snapshot.id);
    }
  });
  await transactionDone(transaction);
}

function enqueueWrite(operation: () => Promise<void>): Promise<void> {
  const next = writeQueue.then(operation, operation).catch(() => {});
  writeQueue = next;
  return next;
}

/** Restore a terminal framebuffer and mark it as recently used. */
export async function loadTerminalSnapshot(
  id: string,
): Promise<TerminalSnapshot | null> {
  try {
    const database = await openDatabase();
    if (!database) return null;
    const transaction = database.transaction(STORE_NAME, "readonly");
    const value = await requestResult(
      transaction.objectStore(STORE_NAME).get(id),
    );
    await transactionDone(transaction);
    if (!isTerminalSnapshot(value)) return null;

    lastUsageStamp = Math.max(lastUsageStamp, value.lastUsedAt);
    if (!cacheWritesEnabled) return null;
    const touched = { ...value, lastUsedAt: nextUsageStamp() };
    void enqueueWrite(() => writeAndPrune(touched));
    return touched;
  } catch {
    return null;
  }
}

/** Save the current xterm framebuffer. Oversized snapshots are not retained. */
export function saveTerminalSnapshot(
  id: string,
  serialized: string,
  cols: number,
  rows: number,
): Promise<void> {
  if (!cacheWritesEnabled) return Promise.resolve();
  const byteSize = serializedByteSize(serialized);
  if (
    !id ||
    !serialized ||
    byteSize > MAX_ENTRY_BYTES ||
    !Number.isInteger(cols) ||
    cols <= 0 ||
    cols > MAX_DIMENSION ||
    !Number.isInteger(rows) ||
    rows <= 0 ||
    rows > MAX_DIMENSION
  ) {
    return deleteTerminalSnapshot(id);
  }
  const entry: TerminalSnapshot = {
    id,
    serialized,
    cols,
    rows,
    byteSize,
    lastUsedAt: nextUsageStamp(),
  };
  return enqueueWrite(() => writeAndPrune(entry));
}

/** Remove one completed or invalid terminal framebuffer. */
export function deleteTerminalSnapshot(id: string): Promise<void> {
  return enqueueWrite(async () => {
    const database = await openDatabase();
    if (!database || !id) return;
    const transaction = database.transaction(STORE_NAME, "readwrite");
    transaction.objectStore(STORE_NAME).delete(id);
    await transactionDone(transaction);
  });
}

/** Remove all locally cached terminal framebuffers, including on sign-out. */
export function clearTerminalCache(): Promise<void> {
  return enqueueWrite(async () => {
    const database = await openDatabase();
    if (!database) return;
    const transaction = database.transaction(STORE_NAME, "readwrite");
    transaction.objectStore(STORE_NAME).clear();
    await transactionDone(transaction);
  });
}

/** Stop new writes and clear snapshots after the server signs the user out. */
export function suspendTerminalCache(): Promise<void> {
  cacheWritesEnabled = false;
  return clearTerminalCache();
}

/** Enable terminal cache writes after a successful in-page sign-in. */
export function enableTerminalCache(): void {
  cacheWritesEnabled = true;
}
