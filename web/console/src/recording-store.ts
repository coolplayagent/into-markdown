const DATABASE = "into-markdown-meeting-recording";
const VERSION = 1;
const META = "meta";
const CHUNKS = "chunks";
const MAX_DRAFT_BYTES = 512 * 1024 * 1024;
const MAX_DRAFT_CHUNKS = 1_000_000;

export class RecordingDraftLimitError extends Error {
  constructor() {
    super("Recording draft exceeds the local Web input limit");
    this.name = "RecordingDraftLimitError";
  }
}

interface StoredMeta {
  key: "current";
  createdAtMs: number;
  elapsedMs: number;
  mimeType: string;
  bytes?: number;
}

interface StoredChunk {
  index: number;
  blob: Blob;
}

export interface RecordingDraft {
  createdAtMs: number;
  elapsedMs: number;
  mimeType: string;
  chunks: Blob[];
}

function request<T>(value: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    value.onsuccess = () => resolve(value.result);
    value.onerror = () => reject(value.error ?? new Error("IndexedDB request failed"));
  });
}

function complete(transaction: IDBTransaction): Promise<void> {
  return new Promise((resolve, reject) => {
    transaction.oncomplete = () => resolve();
    transaction.onabort = () => reject(transaction.error ?? new Error("IndexedDB transaction aborted"));
    transaction.onerror = () => reject(transaction.error ?? new Error("IndexedDB transaction failed"));
  });
}

async function open(): Promise<IDBDatabase> {
  if (!globalThis.indexedDB) throw new Error("IndexedDB is unavailable");
  const pending = indexedDB.open(DATABASE, VERSION);
  pending.onupgradeneeded = () => {
    const database = pending.result;
    if (!database.objectStoreNames.contains(META)) database.createObjectStore(META, { keyPath: "key" });
    if (!database.objectStoreNames.contains(CHUNKS)) database.createObjectStore(CHUNKS, { keyPath: "index" });
  };
  return request(pending);
}

export async function beginRecordingDraft(mimeType: string): Promise<void> {
  if (!mimeType.startsWith("audio/") || mimeType.length > 127) {
    throw new Error("Recording MIME type is invalid");
  }
  const database = await open();
  try {
    const transaction = database.transaction([META, CHUNKS], "readwrite");
    transaction.objectStore(CHUNKS).clear();
    transaction.objectStore(META).put({
      key: "current", createdAtMs: Date.now(), elapsedMs: 0, mimeType, bytes: 0,
    } satisfies StoredMeta);
    await complete(transaction);
  } finally {
    database.close();
  }
}

export async function appendRecordingChunk(index: number, blob: Blob, elapsedMs: number): Promise<void> {
  if (blob.size === 0) return;
  if (!Number.isSafeInteger(index) || index < 0 || index >= MAX_DRAFT_CHUNKS
    || !Number.isSafeInteger(elapsedMs) || elapsedMs < 0) throw new Error("Recording chunk is invalid");
  const database = await open();
  try {
    const read = database.transaction(META, "readonly");
    const metadata = await request(read.objectStore(META).get("current")) as StoredMeta | undefined;
    await complete(read);
    if (!metadata) throw new Error("Recording draft metadata is missing");
    const bytes = (metadata.bytes ?? 0) + blob.size;
    if (!Number.isSafeInteger(bytes) || bytes > MAX_DRAFT_BYTES) {
      throw new RecordingDraftLimitError();
    }
    const transaction = database.transaction([META, CHUNKS], "readwrite");
    transaction.objectStore(CHUNKS).put({ index, blob } satisfies StoredChunk);
    transaction.objectStore(META).put({
      ...metadata, bytes, elapsedMs: Math.max(metadata.elapsedMs, elapsedMs),
    });
    await complete(transaction);
  } finally {
    database.close();
  }
}

export async function loadRecordingDraft(): Promise<RecordingDraft | null> {
  const database = await open();
  try {
    const transaction = database.transaction([META, CHUNKS], "readonly");
    const metadata = await request(transaction.objectStore(META).get("current")) as StoredMeta | undefined;
    const chunks = await request(transaction.objectStore(CHUNKS).getAll()) as StoredChunk[];
    await complete(transaction);
    if (!metadata || chunks.length === 0) return null;
    if (!Number.isSafeInteger(metadata.createdAtMs) || metadata.createdAtMs < 0
      || !Number.isSafeInteger(metadata.elapsedMs) || metadata.elapsedMs < 0
      || !metadata.mimeType.startsWith("audio/") || metadata.mimeType.length > 127
      || chunks.length > MAX_DRAFT_CHUNKS) throw new Error("Recording draft metadata is invalid");
    chunks.sort((left, right) => left.index - right.index);
    let bytes = 0;
    for (const [index, chunk] of chunks.entries()) {
      if (chunk.index !== index || !(chunk.blob instanceof Blob)) {
        throw new Error("Recording draft chunks are incomplete");
      }
      bytes += chunk.blob.size;
      if (!Number.isSafeInteger(bytes) || bytes > MAX_DRAFT_BYTES) {
        throw new RecordingDraftLimitError();
      }
    }
    if (metadata.bytes !== undefined && metadata.bytes !== bytes) {
      throw new Error("Recording draft size does not match its metadata");
    }
    return { createdAtMs: metadata.createdAtMs, elapsedMs: metadata.elapsedMs,
      mimeType: metadata.mimeType, chunks: chunks.map((chunk) => chunk.blob) };
  } finally {
    database.close();
  }
}

export async function clearRecordingDraft(): Promise<void> {
  const database = await open();
  try {
    const transaction = database.transaction([META, CHUNKS], "readwrite");
    transaction.objectStore(META).clear();
    transaction.objectStore(CHUNKS).clear();
    await complete(transaction);
  } finally {
    database.close();
  }
}
