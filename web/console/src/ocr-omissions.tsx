import { useI18n } from "./i18n";

export interface Omission { page?: number; slide?: number; sheet?: string; part?: string; resource?: true }
const MEMORY_CODE = "ocr.optionalRecognitionMemorySkipped";
const RESOURCE_CODE = "ocr.optionalRecognitionResourceSkipped";

export function parseOcrOmissions(text: string): Omission[] {
  const value: unknown = JSON.parse(text);
  if (!value || typeof value !== "object" || !("diagnostics" in value)
    || !Array.isArray(value.diagnostics)) throw new Error("invalid diagnostics");
  return value.diagnostics.filter((item) => item?.code === MEMORY_CODE || item?.code === RESOURCE_CODE).map((item) => {
    const locator = item.locator;
    if (!locator || typeof locator !== "object") throw new Error("missing omission locator");
    const result: Omission = {};
    for (const key of ["page", "slide"] as const) {
      if (Number.isSafeInteger(locator[key]) && locator[key] > 0) result[key] = locator[key];
    }
    for (const key of ["sheet", "part"] as const) {
      if (typeof locator[key] === "string") result[key] = locator[key].slice(0, 1024);
    }
    if (item.code === RESOURCE_CODE) result.resource = true;
    return result;
  });
}

export function OcrOmissions({ omissions }: { omissions: Omission[] }) {
  const { t } = useI18n();
  if (omissions.length === 0) return null;
  const groups = [
    { message: "ocrMemoryOmission" as const, items: omissions.filter((item) => !item.resource) },
    { message: "ocrResourceOmission" as const, items: omissions.filter((item) => item.resource) },
  ];
  return <>{groups.filter((group) => group.items.length > 0).map((group) =>
    <section className="preview-notice" aria-label={t(group.message)} key={group.message}>
      <p>{t(group.message)}</p>
      <ul>
        {group.items.map((item, index) => <li key={index}>
          {item.page && <span>{t("ocrOmissionPage")} {item.page} · </span>}
          {item.slide && <span>{t("ocrOmissionSlide")} {item.slide} · </span>}
          {item.sheet && <span>{item.sheet} · </span>}{item.part}
        </li>)}
      </ul>
    </section>)}</>;
}
