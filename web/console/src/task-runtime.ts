import type { LocalSelection } from "./local-sources";
import type { ApiClient, ArtifactPreview, TaskEvent, TaskRecord, WorkbenchOptions, MeetingOptions } from "./api";
import { ApiError } from "./api";

const terminal = new Set(["succeeded", "failed", "interrupted", "cancelled"]);
export interface UploadEntry {
  key: string;
  file: File;
  localSelection?: LocalSelection;
  localGrantReleased?: boolean;
  waitingForService?: boolean;
  originalSize?: number;
  task?: TaskRecord;
  stage?: string;
  error?: string | undefined;
  batchId?: string;
  submissionId?: string;
  uploadState?: "waiting" | "uploading" | "receiving" | "cancelled" | "reselect";
  uploaded?: number;
  startedAt?: number;
}
type Listener = () => void;
const recoveryKey = "into-md.pending-uploads";

/** One owner for request lifetimes, uploads and task observations across routes. */
export class TaskRuntime {
  entries: UploadEntry[] = [];
  revision = 0;
  historyRevision = 0;
  readonly deletedIds = new Set<string>();
  meetingTask: TaskRecord | null = null;
  meetingUploading = false;
  meetingUploadName = "";
  private meetingSubmissionId: string | undefined;
  private meetingRecovering = false;
  connectionError = false;
  connectionErrorCode: string | undefined;
  private retryOptions = new Map<string, WorkbenchOptions>();
  readonly tasks = new Map<string, TaskRecord>();
  private listeners = new Set<Listener>();
  private observers = new Map<string, Set<(event: TaskEvent) => void>>();
  private controllers = new Map<string, AbortController>();
  private queue: Array<{ key: string; options: WorkbenchOptions; batchId: string } | { meeting: () => void }> = [];
  private active = 0;
  private timer: ReturnType<typeof setTimeout> | undefined;
  private polling = false;
  private restoreRequest: AbortController | undefined;
  private persistTimer: ReturnType<typeof setTimeout> | undefined;
  private failures = 0;
  private stopped = false;
  private request: AbortController | undefined;
  private versions = new Map<string, { generation: string; sequence: number }>();
  private previews = new Map<string, ArtifactPreview>();
  private previewRequests = new Map<string, { controller: AbortController; promise: Promise<ArtifactPreview>; users: number }>();
  readonly api: ApiClient;

  constructor(private readonly source: ApiClient) {
    const overrides: Partial<ApiClient> = {
      uploadMeeting: async (file, options, signal) => {
        const submissionId = crypto.randomUUID().replaceAll("-", "");
        this.meetingSubmissionId = submissionId; this.meetingUploadName = file.name;
        this.meetingUploading = true; this.changed();
        const frozen = structuredClone(options) as MeetingOptions;
        return new Promise<TaskRecord>((resolve, reject) => {
          this.queue.push({ meeting: () => {
            const controller = new AbortController();
            const key = `meeting:${submissionId}`; this.controllers.set(key, controller);
            const abort = () => controller.abort();
            if (signal?.aborted) abort(); else signal?.addEventListener("abort", abort, { once: true });
            void source.uploadMeeting(file, frozen, controller.signal, submissionId).then(record => {
              this.meetingSubmissionId = undefined; this.meetingTask = record; this.put(record); this.wake(); resolve(record);
            }, reject).finally(() => {
              signal?.removeEventListener("abort", abort); this.controllers.delete(key);
              this.meetingUploading = false; this.active -= 1; this.changed(); this.pump();
            });
          } }); this.pump();
        });
      },
      watchTask: (id, callback, signal) => this.watch(id, callback, signal),
      getTask: async (id, signal) => { const record = await source.getTask(id, signal); this.put(record); return record; },
      cancel: async (id, signal) => { const record = await source.cancel(id, signal); this.put(record); this.wake(); return record; },
      retry: async (id, signal) => { const record = await source.retry(id, signal); this.versions.delete(record.id); this.put(record, true); this.wake(); return record; },
      setPinned: async (id, pinned, signal) => { const record = await source.setPinned(id, pinned, signal); this.put(record); return record; },
      relabelSpeakers: async (id, generation, labels, signal) => { const record = await source.relabelSpeakers(id, generation, labels, signal); this.put(record); return record; },
      cleanup: async signal => { const result = await source.cleanup(signal); this.historyRevision += 1; this.changed(); return result; },
      deleteTask: async (id, signal) => { await source.deleteTask(id, signal); this.deletedIds.add(id); this.historyRevision += 1; this.tasks.delete(id); this.entries = this.entries.filter(e => e.task?.id !== id); this.changed(); },
      preview: (id, key, signal) => this.preview(id, key, signal),
    };
    this.api = new Proxy({ ...source }, { get: (_target, property) => Reflect.has(overrides, property) ? Reflect.get(overrides, property) : Reflect.get(source, property, source) });
  }
  subscribe = (listener: Listener) => { this.listeners.add(listener); return () => { this.listeners.delete(listener); }; };
  snapshot = () => this.revision;
  setEntries = (update: UploadEntry[] | ((current: UploadEntry[]) => UploadEntry[])) => {
    this.entries = typeof update === "function" ? update(this.entries) : update;
    this.changed();
  };
  private changed() {
    this.revision += 1;
    this.listeners.forEach(listener => listener());
    clearTimeout(this.persistTimer);
    this.persistTimer = setTimeout(() => this.persist(), 200);
  }
  private persist() {
    try {
      const records = this.entries.filter(e => e.batchId).map(e => ({ key: e.key, name: e.file.name, size: e.originalSize ?? e.file.size, modified: e.file.lastModified, batchId: e.batchId, submissionId: e.submissionId, taskId: e.task?.id, cancelled: e.uploadState === "cancelled" }));
      window.sessionStorage.setItem(recoveryKey, JSON.stringify(records));
      if (this.meetingSubmissionId) window.sessionStorage.setItem(`${recoveryKey}.meeting`, this.meetingSubmissionId);
      else window.sessionStorage.removeItem(`${recoveryKey}.meeting`);
      window.sessionStorage.setItem(`${recoveryKey}.meetingName`, this.meetingUploadName);
    } catch { /* Task recovery through the service remains available. */ }
  }
  start() {
    this.stopped = false;
    window.addEventListener("focus", this.wake);
    document.addEventListener("visibilitychange", this.wake);
    if (!this.entries.length) {
      try {
        this.meetingSubmissionId = window.sessionStorage.getItem(`${recoveryKey}.meeting`) ?? undefined;
        this.meetingRecovering = Boolean(this.meetingSubmissionId);
        this.meetingUploadName = window.sessionStorage.getItem(`${recoveryKey}.meetingName`) ?? "";
        const saved: unknown = JSON.parse(window.sessionStorage.getItem(recoveryKey) ?? "[]");
        if (Array.isArray(saved)) this.entries = saved.slice(0, 10000).flatMap(item => {
          if (typeof item?.key !== "string" || typeof item?.name !== "string" || typeof item?.batchId !== "string") return [];
          const file = new File([], item.name, { lastModified: Number(item.modified) || 0 });
          return [{ key: item.key, file, originalSize: Number.isSafeInteger(item.size) && item.size >= 0 ? item.size : 0, batchId: item.batchId, ...(typeof item.submissionId === "string" ? { submissionId: item.submissionId } : {}), uploadState: item.cancelled === true ? "cancelled" as const : "reselect" as const }];
        });
      } catch { /* Ignore invalid browser recovery metadata. */ }
    }
    void this.restore(); this.wake();
  }
  private async restore() {
    const controller = new AbortController();
    this.restoreRequest?.abort(); this.restoreRequest = controller;
    try {
      for (const entry of this.entries) {
        if (!entry.submissionId || !this.source.receipt) continue;
        try {
          const receipt = await this.source.receipt(entry.submissionId, controller.signal);
          if (receipt.taskId) { const task = await this.source.getTask(receipt.taskId, controller.signal); this.patch(entry.key, { task, error: undefined, waitingForService: false }); this.put(task); }
          else if (receipt.state === "receiving" || receipt.localCopy && ["waiting", "uploading"].includes(receipt.state)) this.patch(entry.key, { uploadState: "receiving" });
        } catch { if (controller.signal.aborted) return; }
      }
      let after: { updatedAtMs: number; id: string } | undefined;
      do {
        const page = await this.source.listTasks({ limit: 100, active: true, ...(after ? { after } : {}) }, controller.signal);
        if (this.stopped) return;
        page.tasks.filter(task => !terminal.has(task.status)).forEach(task => { this.put(task); if (task.workflow === "conversion" && !this.entries.some(e => e.task?.id === task.id)) this.entries.push({ key: task.id, file: new File([], task.displayName ?? task.id), task, ...(task.batchId ? { batchId: task.batchId } : {}) }); }); this.changed(); after = page.nextCursor;
      } while (after);
    } catch { /* The next explicit refresh can recover the service. */ }
    this.wake();
  }
  stop() {
    this.stopped = true; clearTimeout(this.timer); this.request?.abort();
    this.restoreRequest?.abort(); clearTimeout(this.persistTimer); this.persist();
    this.controllers.forEach(controller => controller.abort());
    this.previewRequests.forEach(request => request.controller.abort());
    window.removeEventListener("focus", this.wake);
    document.removeEventListener("visibilitychange", this.wake);
  }
  put(record: TaskRecord, force = false) {
    const old = this.tasks.get(record.id);
    if (!force && old && (old.updatedAtMs > record.updatedAtMs || terminal.has(old.status) && !terminal.has(record.status))) return false;
    const next = old && record.artifacts.length === 0 && old.status === record.status && old.artifactGeneration === record.artifactGeneration ? { ...record, artifacts: old.artifacts } : record;
    if (terminal.has(record.status) && (!old || old.status !== record.status || old.updatedAtMs !== record.updatedAtMs || old.pinned !== record.pinned)) this.historyRevision += 1;
    this.tasks.set(record.id, next);
    if (record.workflow === "meetingTranscript" && (!this.meetingTask || this.meetingTask.id === record.id || record.createdAtMs > this.meetingTask.createdAtMs)) this.meetingTask = next;
    this.entries = this.entries.map(entry => entry.task?.id === record.id ? { ...entry, task: next } : entry);
    this.changed(); return true;
  }
  submit(keys: string[], options: WorkbenchOptions, batchId: string) {
    const frozen = structuredClone(options);
    this.entries = this.entries.map(entry => keys.includes(entry.key) ? { ...entry, error: undefined, batchId, submissionId: crypto.randomUUID().replaceAll("-", ""), uploadState: "waiting" as const } as UploadEntry : entry);
    keys.forEach(key => this.retryOptions.set(key, frozen));
    this.queue.push(...keys.map(key => ({ key, options: frozen, batchId })));
    this.changed(); this.pump();
  }
  canRetryUpload(key: string) { return this.retryOptions.has(key) && !this.entries.find(e => e.key === key)?.localGrantReleased && !["localSelectionExpired", "localSourceChanged", "localFileUnavailable"].includes(this.entries.find(e => e.key === key)?.error ?? ""); }
  retryUpload(key: string) {
    const entry = this.entries.find(e => e.key === key);
    const options = this.retryOptions.get(key);
    if (!entry || !options || !entry.batchId) return;
    const reconcile = async () => {
      if (entry.submissionId && this.source.receipt) {
        const receipt = await this.source.receipt(entry.submissionId).catch(error => {
          if (error instanceof ApiError && error.code === "notFound") return null;
          throw error;
        });
        if (receipt?.taskId) { const task = await this.api.getTask(receipt.taskId); this.patch(key, { task, error: undefined, waitingForService: false }); this.wake(); return; }
        if (receipt?.state === "receiving" || receipt?.localCopy && ["waiting", "uploading"].includes(receipt.state)) { this.patch(key, { error: undefined, uploadState: "receiving" }); this.wake(); return; }
        if (receipt) await this.source.cancelUpload?.(entry.submissionId);
      }
      this.submit([key], options, entry.batchId!);
    };
    void reconcile().catch(() => this.patch(key, { error: "unreachable" }));
  }
  cancelUpload(key: string) {
    this.queue = this.queue.filter(item => "meeting" in item || item.key !== key);
    this.controllers.get(key)?.abort();
    const entry = this.entries.find(e => e.key === key);
    if (entry?.localSelection && !entry.task) {
      this.patch(key, { localGrantReleased: true });
      void this.source.releaseLocal?.(entry.localSelection.id).catch(() => this.patch(key, { error: "localFileUnavailable" }));
    }
    if (entry?.submissionId && entry.uploadState !== "waiting") void this.source.cancelUpload?.(entry.submissionId).catch(error => {
      if (!(error instanceof ApiError && error.code === "notFound")) this.patch(key, { error: "unreachable" });
    });
    this.setEntries(items => items.map(e => e.key === key ? { ...e, uploadState: "cancelled", waitingForService: false } : e));
  }
  private pump() {
    while (this.active < 1 && this.queue.length && !this.stopped) {
      const job = this.queue.shift()!;
      if ("meeting" in job) { this.active += 1; job.meeting(); continue; }
      const entry = this.entries.find(e => e.key === job.key);
      if (!entry) continue;
      this.active += 1;
      const controller = new AbortController(); this.controllers.set(job.key, controller);
      this.patch(job.key, { uploadState: "uploading", uploaded: 0, startedAt: Date.now() });
      const progress = (loaded: number) => this.patch(job.key, { uploaded: loaded, uploadState: loaded >= (entry.originalSize ?? entry.file.size) ? "receiving" : "uploading" });
      const waiting = (value: boolean) => this.patch(job.key, { waitingForService: value, error: undefined });
      const upload = entry.localSelection && this.source.importLocal
        ? this.source.importLocal(entry.localSelection, job.options, job.batchId, progress, controller.signal, entry.submissionId, waiting)
        : this.source.uploadProgress
        ? this.source.uploadProgress(entry.file, job.options, job.batchId, progress, controller.signal, entry.submissionId, waiting)
        : this.source.upload(entry.file, job.options, job.batchId, controller.signal);
      void upload.then(task => {
        this.patch(job.key, { task, stage: task.status, error: undefined, waitingForService: false }); this.put(task); this.wake();
      }).catch(error => {
        this.patch(job.key, controller.signal.aborted ? { uploadState: "cancelled" } : { error: error instanceof ApiError ? error.code : "unreachable" });
      }).finally(() => { this.active -= 1; this.controllers.delete(job.key); this.changed(); this.pump(); });
    }
  }
  private patch(key: string, patch: Partial<UploadEntry>) { this.setEntries(items => items.map(e => e.key === key ? { ...e, ...patch } : e)); }
  get uploading() { return this.active > 0 || this.queue.length > 0; }
  private watch(id: string, callback: (event: TaskEvent) => void, signal: AbortSignal): Promise<void> {
    if (signal.aborted) return Promise.resolve();
    return new Promise(resolve => {
      const listeners = this.observers.get(id) ?? new Set();
      const finish = () => { listeners.delete(receive); if (!listeners.size) this.observers.delete(id); signal.removeEventListener("abort", finish); resolve(); };
      const receive = (event: TaskEvent) => { callback(event); if (event.terminal) finish(); };
      listeners.add(receive); this.observers.set(id, listeners); signal.addEventListener("abort", finish, { once: true }); this.wake();
    });
  }
  wake = () => { clearTimeout(this.timer); if (!this.stopped && !this.polling) this.timer = setTimeout(() => void this.poll(), 0); };
  private async poll() {
    if (this.stopped || this.polling) return;
    this.polling = true;
    const controller = new AbortController(); this.request = controller;
    try {
      if (this.meetingRecovering && this.meetingSubmissionId && this.source.receipt && !this.controllers.has(`meeting:${this.meetingSubmissionId}`)) {
        const receipt = await this.source.receipt(this.meetingSubmissionId, controller.signal);
        if (receipt.taskId) {
          this.put(await this.source.getTask(receipt.taskId, controller.signal));
          this.meetingSubmissionId = undefined; this.meetingRecovering = false; this.meetingUploading = false; this.changed();
        } else if (receipt.state === "receiving") { this.meetingUploading = true; }
        else { this.meetingSubmissionId = undefined; this.meetingRecovering = false; this.meetingUploading = false; this.changed(); }
      }
      for (const entry of this.entries.filter(e => !e.task && e.uploadState === "receiving" && e.submissionId && !this.controllers.has(e.key)).slice(0, 2)) {
        if (!this.source.receipt) continue;
        const receipt = await this.source.receipt(entry.submissionId!, controller.signal);
        if (receipt.taskId) { const task = await this.source.getTask(receipt.taskId, controller.signal); this.patch(entry.key, { task, error: undefined, waitingForService: false }); this.put(task); }
        else if (["failed", "interrupted", "cancelled"].includes(receipt.state)) this.patch(entry.key, { uploadState: "reselect", error: receipt.error ?? "uploadFailed" });
      }
      const ids = [...new Set([...this.observers.keys(), ...[...this.tasks.values()].filter(task => !terminal.has(task.status)).map(task => task.id)])];
      for (let offset = 0; offset < ids.length && !this.stopped; offset += 100) {
        const group = ids.slice(offset, offset + 100);
        const records = this.source.summaries ? await this.source.summaries(group, controller.signal) : await Promise.all(group.map(id => this.source.getTask(id, controller.signal)));
        for (const record of records) {
          if ("generation" in record) {
            const version = record as TaskRecord & { generation: string; sequence: number; execution?: TaskEvent["execution"] };
            const old = this.versions.get(record.id);
            if (old?.generation === version.generation && old.sequence > version.sequence) continue;
            this.versions.set(record.id, { generation: version.generation, sequence: version.sequence });
          }
          if (!this.put(record)) continue;
          const execution = (record as TaskRecord & { execution?: TaskEvent["execution"] }).execution;
          const event: TaskEvent = { schemaVersion: 1, sequence: this.versions.get(record.id)?.sequence ?? 0, taskId: record.id, kind: "snapshot", status: record.status, progressMillionths: record.progressMillionths, terminal: terminal.has(record.status), ...(execution ? { execution } : {}) };
          this.entries = this.entries.map(entry => entry.task?.id === record.id ? { ...entry, stage: (record as TaskRecord & { waitingReason?: string }).waitingReason === "disk" ? "waitingDisk" : (record as TaskRecord & { waitingReason?: string }).waitingReason === "worker" ? "waitingWorker" : execution?.stage ?? record.status } : entry);
          this.observers.get(record.id)?.forEach(listener => listener(event));
        }
      }
      this.failures = 0;
      if (this.connectionError) { this.connectionError = false; this.connectionErrorCode = undefined; this.changed(); }
    } catch (error) { if (!controller.signal.aborted) { this.failures += 1; this.connectionError = true; this.connectionErrorCode = error instanceof ApiError ? error.code : undefined; this.changed(); } }
    finally {
      this.polling = false;
      if (!this.stopped && (this.meetingRecovering || this.observers.size || [...this.tasks.values()].some(task => !terminal.has(task.status)) || this.entries.some(entry => !entry.task && entry.uploadState === "receiving"))) this.timer = setTimeout(() => void this.poll(), this.failures ? Math.min(30000, 1000 * 2 ** this.failures) : document.hidden ? 5000 : 1000);
    }
  }
  private async preview(id: string, key: string, signal?: AbortSignal): Promise<ArtifactPreview> {
    const task = this.tasks.get(id);
    const cacheKey = `${id}:${task?.artifactGeneration ?? 0}:${key}:${task?.artifacts.find(a => a.storageKey === key)?.sha256 ?? ""}`;
    if (signal?.aborted) throw new DOMException("Aborted", "AbortError");
    const cached = this.previews.get(cacheKey); if (cached) return cached;
    let request = this.previewRequests.get(cacheKey);
    if (request?.controller.signal.aborted) { this.previewRequests.delete(cacheKey); request = undefined; }
    if (!request) {
      const controller = new AbortController();
      const promise = this.source.preview(id, key, controller.signal).then(value => {
        if (this.previews.size >= 32) this.previews.delete(this.previews.keys().next().value!);
        this.previews.set(cacheKey, value); return value;
      }).finally(() => { if (this.previewRequests.get(cacheKey)?.controller === controller) this.previewRequests.delete(cacheKey); });
      request = { controller, promise, users: 0 }; this.previewRequests.set(cacheKey, request);
    }
    const shared = request; shared.users += 1;
    return new Promise((resolve, reject) => {
      let done = false;
      const finish = () => { if (done) return false; done = true; signal?.removeEventListener("abort", abort); if (--shared.users === 0) shared.controller.abort(); return true; };
      const abort = () => { if (finish()) reject(new DOMException("Aborted", "AbortError")); };
      signal?.addEventListener("abort", abort, { once: true });
      shared.promise.then(value => { if (finish()) resolve(value); }, error => { if (finish()) reject(error); });
    });
  }
}
