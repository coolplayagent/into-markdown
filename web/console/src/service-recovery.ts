import { ApiError } from "./api";
import { abortableDelay } from "./local-sources";

export type ServiceWait = (waiting: boolean) => void;
export function transientServiceError(error: unknown): boolean {
  return error instanceof ApiError && ["unreachable", "requestTimeout"].includes(error.code);
}

/** Retry observation or an idempotent receipt creation; never replay file bytes here. */
export async function awaitService<T>(operation: () => Promise<T>, signal: AbortSignal, waiting?: ServiceWait): Promise<T> {
  let failures = 0;
  for (;;) {
    signal.throwIfAborted();
    try {
      const value = await operation();
      waiting?.(false);
      return value;
    } catch (error) {
      if (signal.aborted || !transientServiceError(error)) { waiting?.(false); throw error; }
      waiting?.(true);
      await abortableDelay(Math.min(5000, 500 * 2 ** Math.min(failures++, 4)), signal);
    }
  }
}
