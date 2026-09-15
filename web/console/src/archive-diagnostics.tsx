import { useEffect, useState } from "react";
import type { ApiClient, TaskRecord } from "./api";
import { useI18n } from "./i18n";
import { OcrOmissions, parseOcrOmissions, type Omission } from "./ocr-omissions";

export function archiveMembers(text: string): string[] {
  const value: unknown = JSON.parse(text);
  if (!value || typeof value !== "object" || !("diagnostics" in value) || !Array.isArray(value.diagnostics)) return [];
  return value.diagnostics.slice(0, 1024).flatMap((item: unknown) => {
    if (!item || typeof item !== "object" || !("code" in item) || item.code !== "zip.entry.archiveExtractionRequired" || !("locator" in item)) return [];
    const locator = item.locator;
    return locator && typeof locator === "object" && "part" in locator && typeof locator.part === "string" ? [locator.part.length > 4096 ? `${locator.part.slice(0, 4096)}…` : locator.part] : [];
  });
}

export interface ConversionObservations {
  outcome?: "complete" | "degraded";
  reasons: string[];
  ocr?: { imageSources: number; imagesAttempted: number; imagesCompleted: number; imagesFailed: number; imagesSkipped: number };
}
export function conversionObservations(text: string): ConversionObservations {
  const value = JSON.parse(text);
  const result: ConversionObservations = { reasons: [] };
  if (value?.outcome === "complete" || value?.outcome === "degraded") result.outcome = value.outcome;
  if (Array.isArray(value?.diagnostics)) result.reasons = value.diagnostics.flatMap((item: any) => {
    if (item?.severity !== "warning" || typeof item.message !== "string") return [];
    const page = item.locator?.page;
    return [`${Number.isSafeInteger(page) ? `[${page}] ` : ""}${item.message}`];
  });
  const ocr = value?.ocrRuntime;
  if (ocr && ["imageSources", "imagesAttempted", "imagesCompleted", "imagesFailed", "imagesSkipped"].every((key) => Number.isSafeInteger(ocr[key]) && ocr[key] >= 0)) result.ocr = ocr;
  return result;
}

export function ArchiveDiagnostics({ api, task }: { api: ApiClient; task: TaskRecord }) {
  const { t, locale } = useI18n();
  const [omissions, setOmissions] = useState<Omission[]>([]);
  const [observations, setObservations] = useState<ConversionObservations>({ reasons: [] });
  const [members, setMembers] = useState<string[]>([]);
  const [unavailable, setUnavailable] = useState(false);
  const artifact = task.artifacts.find((item) => item.kind === "diagnostics");
  useEffect(() => {
    const controller = new AbortController();
    setMembers([]); setOmissions([]); setObservations({ reasons: [] }); setUnavailable(false);
    if (artifact) void api.preview(task.id, artifact.storageKey, controller.signal).then(async (preview) => {
      if (controller.signal.aborted) return;
      const text = preview.truncated
        ? await (await api.download(task.id, artifact.storageKey, controller.signal)).blob.text()
        : preview.text;
      if (controller.signal.aborted) return;
      setMembers(archiveMembers(text));
      setObservations(conversionObservations(text));
      if (task.status === "succeeded") setOmissions(parseOcrOmissions(text));
    }).catch(() => { if (!controller.signal.aborted) setUnavailable(true); });
    return () => controller.abort();
  }, [api, task.id, task.status, artifact?.storageKey]);
  if (unavailable) return <p role="status">{t("diagnosticsPreviewUnavailable")}</p>;
  return <>{task.status === "succeeded" && <aside className="preview-notice" aria-label={t("diagnostics")}>
    {observations.outcome === "degraded" && <><strong>{locale === "zh-CN" ? "转换完成，部分内容已降级" : "Conversion completed with some degraded content"}</strong>
      <ul>{observations.reasons.map((reason, index) => <li key={index}>{reason}</li>)}</ul></>}
    {observations.ocr && <p>{locale === "zh-CN"
      ? `OCR 图片总数 ${observations.ocr.imageSources}；尝试 ${observations.ocr.imagesAttempted}，完成 ${observations.ocr.imagesCompleted}，失败 ${observations.ocr.imagesFailed}，跳过 ${observations.ocr.imagesSkipped}`
      : `OCR images ${observations.ocr.imageSources}; attempted ${observations.ocr.imagesAttempted}, completed ${observations.ocr.imagesCompleted}, failed ${observations.ocr.imagesFailed}, skipped ${observations.ocr.imagesSkipped}`}</p>}
  </aside>}<OcrOmissions omissions={omissions} />{members.length ? <aside className="preview-notice" aria-label={t("diagnostics")}><ul>{members.map((member, index) => <li key={index}><strong>{member}</strong>: {t("archiveExtractionRequired")}</li>)}</ul></aside> : null}</>;
}
