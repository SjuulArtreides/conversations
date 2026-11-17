export type SerialOperation = (done: () => void) => void;

/** Serialize callback-based mutations whose completion may be asynchronous. */
export function createSerialOperationQueue() {
  const operations: SerialOperation[] = [];
  let running = false;
  let disposed = false;

  const drain = () => {
    if (disposed || running) return;
    const operation = operations.shift();
    if (!operation) return;
    running = true;
    operation(() => {
      if (disposed) return;
      running = false;
      drain();
    });
  };

  return {
    enqueue(operation: SerialOperation) {
      if (disposed) return;
      operations.push(operation);
      drain();
    },
    dispose() {
      disposed = true;
      operations.length = 0;
    },
  };
}
