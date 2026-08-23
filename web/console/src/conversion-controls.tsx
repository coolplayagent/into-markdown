import { CheckCircle2, CircleAlert, FileText, LoaderCircle, ScanText } from "lucide-react";
import type { CapabilityAdmin, ComponentStatus, WorkbenchOptions } from "./api";
import { useI18n } from "./i18n";
import { RouteLink } from "./router";
import { capabilitySourceLabel } from "./source-label";

function SegmentedControl<T extends string>({
  label, value, items, onChange,
}: {
  label: string;
  value: T;
  items: Array<{ value: T; label: string; description: string; recommended?: boolean }>;
  onChange(value: T): void;
}) {
  const { t } = useI18n();
  return <div className="segmented-field">
    <span>{label}</span>
    <div className={`segmented-control ${items.length === 2 ? "two" : ""}`} role="group" aria-label={label}>
      {items.map((item) => <button key={item.value} type="button" aria-pressed={value === item.value} onClick={() => onChange(item.value)}>
        {item.label}{item.recommended && <small>{t("recommended")}</small>}
      </button>)}
    </div>
    <small className="segmented-help" aria-live="polite">{items.find((item) => item.value === value)?.description}</small>
  </div>;
}

export function CapabilityStrip({ ocr, capability }: { ocr?: ComponentStatus | undefined; capability?: CapabilityAdmin | undefined }) {
  const { locale, t } = useI18n();
  return <section className="capability-strip" aria-label={t("capabilities")}>
    <div className="capability-item">
      <span className="capability-icon"><FileText size={21} aria-hidden="true" /></span>
      <div><strong>{t("documentParsing")}</strong><span className="ready"><CheckCircle2 size={14} aria-hidden="true" />{t("localReady")}</span></div>
    </div>
    <div className="capability-item">
      <span className="capability-icon"><ScanText size={21} aria-hidden="true" /></span>
      <div><strong>{t("imageOcr")}</strong>{!ocr || ["unknown", "checking", "verifying"].includes(ocr.code)
        ? <span className="checking"><LoaderCircle className="spin" size={14} aria-hidden="true" />{t("checkingSystem")}</span>
        : ocr.available
        ? <span className="ready"><CheckCircle2 size={14} aria-hidden="true" />{capabilitySourceLabel(capability?.currentSource, locale, ocr.detail)}</span>
        : <><span className="needs-setup"><CircleAlert size={14} aria-hidden="true" />{t("sourceNeeded")}</span><RouteLink href="/admin/capabilities" className="capability-install">{locale === "zh-CN" ? "前往设置" : "Open settings"}</RouteLink></>}
      </div>
    </div>
  </section>;
}

export function OptionPanel({ value, onChange, disabled }: { value: WorkbenchOptions; onChange(value: WorkbenchOptions): void; disabled: boolean }) {
  const { t } = useI18n();
  const patch = <K extends keyof WorkbenchOptions>(key: K, next: WorkbenchOptions[K]) => onChange({ ...value, [key]: next });
  return <section className="control-card" aria-labelledby="conversion-settings-heading">
    <div className="control-card-heading"><h2 id="conversion-settings-heading">{t("conversionSettings")}</h2></div>
    <fieldset className="quick-option-grid" disabled={disabled}>
      <div className="segmented-field"><span>{t("outputFormat")}</span><div className="segmented-control single" aria-label={t("outputFormat")}><button type="button" aria-pressed="true"><FileText size={16} aria-hidden="true" /> Markdown</button></div></div>
      <SegmentedControl label={t("recognitionMode")} value={value.ocrPolicy} onChange={(next) => patch("ocrPolicy", next)} items={[{ value: "auto", label: t("automatic"), description: t("ocrAutomaticHelp"), recommended: true }, { value: "always", label: t("forceRecognition"), description: t("ocrAlwaysHelp") }, { value: "off", label: t("off"), description: t("ocrOffHelp") }]} />
      <SegmentedControl label={t("assetMode")} value={value.assetMode === "embed" ? "extract" : value.assetMode} onChange={(next) => patch("assetMode", next)} items={[{ value: "extract", label: t("separateAssets"), description: t("assetExtractHelp"), recommended: true }, { value: "omit", label: t("omitAssets"), description: t("assetOmitHelp") }]} />
    </fieldset>
  </section>;
}
