import { useI18n } from "./i18n";

export interface Omission { page?: number; slide?: number; sheet?: string; part?: string }
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

export function OcrOmissions({ omissions }: { omissions: Omission[] }) {
  const { t } = useI18n();
  if (omissions.length === 0) return null;
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
