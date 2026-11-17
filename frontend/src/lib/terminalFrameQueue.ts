import {
  createSerialOperationQueue,
  type SerialOperation,
} from "./serialOperationQueue";

type Frame = { cols: number; rows: number; bytes: Uint8Array };
type Batch = { cols: number; rows: number; parts: Uint8Array[]; size: number };

/** Coalesce waiting frames without crossing resize or reset boundaries. */
export function createTerminalFrameQueue() {
  const queue = createSerialOperationQueue();
  let tail: Batch | null = null;
  return {
    enqueue(operation: SerialOperation) {
      tail = null;
      queue.enqueue(operation);
    },
    frame(
      frame: Frame,
      write: (frame: Frame, count: number, done: () => void) => void,
    ) {
      if (
        tail &&
        tail.cols === frame.cols &&
        tail.rows === frame.rows &&
        tail.size + frame.bytes.length <= 256 * 1024
      ) {
        tail.parts.push(frame.bytes);
        tail.size += frame.bytes.length;
        return;
      }
      const batch = {
        cols: frame.cols,
        rows: frame.rows,
        parts: [frame.bytes],
        size: frame.bytes.length,
      };
      tail = batch;
      queue.enqueue((done) => {
        if (tail === batch) tail = null;
        let bytes = batch.parts[0];
        if (batch.parts.length > 1) {
          bytes = new Uint8Array(batch.size);
          let offset = 0;
          for (const part of batch.parts) {
            bytes.set(part, offset);
            offset += part.length;
          }
        }
        write(
          { cols: batch.cols, rows: batch.rows, bytes },
          batch.parts.length,
          done,
        );
      });
    },
    dispose() {
      tail = null;
      queue.dispose();
    },
  };
}
