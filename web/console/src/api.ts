const MAX_RESPONSE_BYTES = 64 * 1024;

export interface ComponentStatus {
  available: boolean;
  code: string;
  detail: string;
}

export interface StatusResponse {
  schemaVersion: 1;
  localApi: ComponentStatus;
  documentConsole: ComponentStatus;
}

export class ApiError extends Error {
  constructor(readonly code: string) {
    super("The local API request failed.");
    this.name = "ApiError";
  }
}

function isComponent(value: unknown): value is ComponentStatus {
  if (typeof value !== "object" || value === null) return false;
  const component = value as Record<string, unknown>;
  return (
    typeof component.available === "boolean" &&
    typeof component.code === "string" &&
    typeof component.detail === "string"
  );
}

function parseStatus(value: unknown): StatusResponse {
  if (typeof value !== "object" || value === null) throw new ApiError("invalidResponse");
  const record = value as Record<string, unknown>;
  if (
    record.schemaVersion !== 1 ||
    !isComponent(record.localApi) ||
    !isComponent(record.documentConsole)
  ) {
    throw new ApiError("invalidResponse");
  }
  return value as StatusResponse;
}

async function readBoundedJson(response: Response): Promise<unknown> {
  const declaredLength = Number(response.headers.get("content-length"));
  if (Number.isFinite(declaredLength) && declaredLength > MAX_RESPONSE_BYTES) {
    throw new ApiError("responseTooLarge");
  }
  if (!response.body) throw new ApiError("invalidResponse");
  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let length = 0;
  try {
    while (true) {
      const result = await reader.read();
      if (result.done) break;
      length += result.value.byteLength;
      if (length > MAX_RESPONSE_BYTES) {
        await reader.cancel();
        throw new ApiError("responseTooLarge");
      }
      chunks.push(result.value);
    }
  } finally {
    reader.releaseLock();
  }
  const bytes = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.byteLength;
  }
  try {
    return JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes));
  } catch {
    throw new ApiError("invalidResponse");
  }
}

export interface ApiClient {
  status(signal?: AbortSignal): Promise<StatusResponse>;
}

export function createApiClient(session: string, fetcher: typeof fetch = fetch): ApiClient {
  return Object.freeze({
    async status(signal?: AbortSignal): Promise<StatusResponse> {
      let response: Response;
      try {
        response = await fetcher("/api/status", {
          method: "POST",
          headers: { "X-Into-Md-Session": session },
          body: null,
          cache: "no-store",
          credentials: "omit",
          redirect: "error",
          referrerPolicy: "no-referrer",
          ...(signal ? { signal } : {}),
        });
      } catch {
        throw new ApiError("unreachable");
      }
      const mediaType = response.headers.get("content-type")?.split(";", 1)[0]?.trim();
      if (mediaType !== "application/json") throw new ApiError("invalidResponse");
      const value = await readBoundedJson(response);
      if (!response.ok) {
        const code =
          typeof value === "object" && value !== null && typeof (value as Record<string, unknown>).code === "string"
            ? String((value as Record<string, unknown>).code)
            : "requestFailed";
        throw new ApiError(code);
      }
      return parseStatus(value);
    },
  });
}
