import { useEffect, useState } from "react";
import type { ApiClient, TaskRecord } from "./api";
import { useI18n } from "./i18n";

interface Omission { page?: number; slide?: number; sheet?: string; part?: string }
const CODE = "ocr.optionalRecognitionMemorySkipped";

export function parseOcrOmissions(text: string): Omission[] {
  const value: unknown = JSON.parse(text);
  if (!value || typeof value !== "object" || !("diagnostics" in value)
    || !Array.isArray(value.diagnostics)) throw new Error("invalid diagnostics");
  return value.diagnostics.filter((item) => item?.code === CODE).map((item) => {
    const locator = item.locator;
    if (!locator || typeof locator !== "object") throw new Error("missing omission locator");
    const result: Omission = {};
    for (const key of ["page", "slide"] as const) {
      if (Number.isSafeInteger(locator[key]) && locator[key] > 0) result[key] = locator[key];
    }
    for (const key of ["sheet", "part"] as const) {
      if (typeof locator[key] === "string") result[key] = locator[key].slice(0, 1024);
    }
    return result;
  });
}

export function OcrOmissions({ api, task }: { api: ApiClient; task: TaskRecord }) {
  const { t } = useI18n();
  const [omissions, setOmissions] = useState<Omission[]>([]);
  const [failed, setFailed] = useState(false);
  const artifact = task.artifacts.find((item) => item.kind === "diagnostics");
  const key = artifact?.storageKey;
  const relevant = task.status === "succeeded" && !!key;
  useEffect(() => {
    setOmissions([]); setFailed(false);
    if (!key || !relevant) return;
    const controller = new AbortController();
    void api.preview(task.id, key, controller.signal).then((preview) => {
      if (controller.signal.aborted) return;
      if (preview.truncated) throw new Error("diagnostics preview truncated");
      setOmissions(parseOcrOmissions(preview.text));
    }).catch(() => { if (!controller.signal.aborted) setFailed(true); });
    return () => controller.abort();
  }, [api, task.id, key, relevant]);
  if (!relevant || (!failed && omissions.length === 0)) return null;
  if (failed) return <p className="preview-notice">{t("ocrOmissionDetailsUnavailable")}</p>;
  return <section className="preview-notice" aria-label={t("ocrMemoryOmission")}>
    <p>{t("ocrMemoryOmission")}</p>
    <ul>
      {omissions.map((item, index) => <li key={index}>
        {item.page && <span>{t("ocrOmissionPage")} {item.page} · </span>}
        {item.slide && <span>{t("ocrOmissionSlide")} {item.slide} · </span>}
        {item.sheet && <span>{item.sheet} · </span>}{item.part}
      </li>)}
    </ul>
  </section>;
}
