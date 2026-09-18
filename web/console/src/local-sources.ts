import { ApiError } from "./api";
export interface LocalSelection { id: string; name: string; size: number }
type Requester = (path: string, init: RequestInit, limit?: number) => Promise<unknown>;
export function abortableDelay(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    const abort = () => { clearTimeout(timer); signal.removeEventListener("abort", abort); reject(new DOMException("Aborted", "AbortError")); };
    const timer = setTimeout(() => { signal.removeEventListener("abort", abort); resolve(); }, ms);
    signal.addEventListener("abort", abort, { once: true }); if (signal.aborted) abort();
  });
}
export function localSourceClient(request: Requester, auth: () => Record<string, string>) {
  return {
    async localAvailable(signal?: AbortSignal): Promise<boolean> {
      const value = await request("/api/local-sources", { headers: auth(), ...(signal ? { signal } : {}) });
      return typeof value === "object" && value !== null && "available" in value && value.available === true;
    },
    async selectLocal(kind: "pick" | "paste", signal: AbortSignal): Promise<LocalSelection[]> {
      const value = await request(`/api/local-sources/${kind}`, { method: "POST", headers: auth(), signal });
      if (!value || typeof value !== "object" || !("id" in value) || typeof value.id !== "string" || !/^[A-Za-z0-9_-]{43}$/.test(value.id)) throw new ApiError("invalidResponse");
      const path = `/api/local-sources/operations/${value.id}`;
      let delivered = false;
      try {
        for (;;) {
          const operation = await request(path, { headers: auth(), signal });
          if (!operation || typeof operation !== "object" || !("state" in operation)) throw new ApiError("invalidResponse");
          if (operation.state === "failed") throw new ApiError("error" in operation && typeof operation.error === "string" ? operation.error : "localSourceUnavailable");
          if (operation.state === "ready") {
            if (!("files" in operation) || !Array.isArray(operation.files) || !operation.files.every(validSelection)) throw new ApiError("invalidResponse");
            delivered = true; return operation.files;
          }
          await abortableDelay(250, signal);
        }
      } finally {
        if (!delivered) void request(path, { method: "DELETE", headers: auth() }).catch(() => {});
      }
    },
    async releaseLocal(id: string): Promise<void> { await request(`/api/local-sources/selections/${encodeURIComponent(id)}`, { method: "DELETE", headers: auth() }); },
  };
}
function validSelection(value: unknown): value is LocalSelection {
  return typeof value === "object" && value !== null && "id" in value && typeof value.id === "string" && /^[A-Za-z0-9_-]{43}$/.test(value.id)
    && "name" in value && typeof value.name === "string" && value.name.length > 0 && value.name.length <= 1024
    && "size" in value && Number.isSafeInteger(value.size) && Number(value.size) >= 0;
}
