import {
  CheckCircle2, ChevronDown, CircleAlert, FileText, LoaderCircle, PanelRightOpen, ScanText, SlidersHorizontal,
  Wifi, WifiOff, X,
} from "lucide-react";
import { useState } from "react";
import type { AiMode, CapabilityAdmin, ComponentStatus, InputFormat, NetworkMode, WorkbenchOptions } from "./api";
import { useI18n } from "./i18n";
import { RouteLink } from "./router";
import { capabilitySourceLabel } from "./source-label";
import { FORMATS } from "./task-ui";

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
    <div className="segmented-control" role="group" aria-label={label}>
      {items.map((item) => <button key={item.value} type="button" aria-pressed={value === item.value} onClick={() => onChange(item.value)}>
        {item.label}{item.recommended && <small>{t("recommended")}</small>}
      </button>)}
    </div>
    <small className="segmented-help" aria-live="polite">{items.find((item) => item.value === value)?.description}</small>
  </div>;
}

export function CapabilityStrip({ ocr, capability, onInstallOcr }: { ocr?: ComponentStatus | undefined; capability?: CapabilityAdmin | undefined; onInstallOcr(): Promise<void> }) {
  const { locale, t } = useI18n();
  const [installing, setInstalling] = useState(false);
  const [error, setError] = useState(false);
  const install = async () => {
    setInstalling(true); setError(false);
    try { await onInstallOcr(); } catch { setError(true); } finally { setInstalling(false); }
  };
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
        : <><span className="needs-setup"><CircleAlert size={14} aria-hidden="true" />{t("sourceNeeded")}</span><span className="capability-actions"><button className="capability-install" type="button" disabled={installing} onClick={() => void install()}>{installing ? <LoaderCircle className="spin" size={14} /> : null}{t(installing ? "installingComponents" : "installLocalOcr")}</button><RouteLink href="/admin/capabilities" className="capability-install">{t("chooseAiService")}</RouteLink></span>{error && <small className="capability-error" role="status" aria-live="assertive">{t("installComponentsFailed")}</small>}</>}
      </div>
    </div>
  </section>;
}

export function OptionPanel({ value, onChange, disabled, onOpenAdvanced }: { value: WorkbenchOptions; onChange(value: WorkbenchOptions): void; disabled: boolean; onOpenAdvanced(): void }) {
  const { t } = useI18n();
  const patch = <K extends keyof WorkbenchOptions>(key: K, next: WorkbenchOptions[K]) => onChange({ ...value, [key]: next });
  return <section className="control-card" aria-labelledby="conversion-settings-heading">
    <div className="control-card-heading"><h2 id="conversion-settings-heading">{t("conversionSettings")}</h2></div>
    <fieldset className="quick-option-grid" disabled={disabled}>
      <div className="segmented-field"><span>{t("outputFormat")}</span><div className="segmented-control single" aria-label={t("outputFormat")}><button type="button" aria-pressed="true"><FileText size={16} aria-hidden="true" /> Markdown</button></div></div>
      <SegmentedControl label={t("recognitionMode")} value={value.ocrPolicy} onChange={(next) => patch("ocrPolicy", next)} items={[{ value: "auto", label: t("automatic"), description: t("ocrAutomaticHelp"), recommended: true }, { value: "always", label: t("forceRecognition"), description: t("ocrAlwaysHelp") }, { value: "off", label: t("off"), description: t("ocrOffHelp") }]} />
      <SegmentedControl label={t("assetMode")} value={value.assetMode} onChange={(next) => patch("assetMode", next)} items={[{ value: "extract", label: t("separateAssets"), description: t("assetExtractHelp"), recommended: true }, { value: "embed", label: t("embedAssets"), description: t("assetEmbedHelp") }, { value: "omit", label: t("omitAssets"), description: t("assetOmitHelp") }]} />
    </fieldset>
    <button className="advanced-trigger" type="button" onClick={onOpenAdvanced}><SlidersHorizontal size={17} aria-hidden="true" /><span>{t("openAdvancedSettings")}</span><PanelRightOpen size={17} aria-hidden="true" /></button>
  </section>;
}

export function AdvancedSettings({ value, onChange, open, onClose, providerCapabilityActive = false }: { value: WorkbenchOptions; onChange(value: WorkbenchOptions): void; open: boolean; onClose(): void; providerCapabilityActive?: boolean }) {
  const { t } = useI18n();
  const patch = <K extends keyof WorkbenchOptions>(key: K, next: WorkbenchOptions[K]) => onChange({ ...value, [key]: next });
  if (!open) return null;
  return <div className="sheet-backdrop" role="presentation" onMouseDown={(event) => { if (event.currentTarget === event.target) onClose(); }}>
    <aside className="settings-sheet" role="dialog" aria-modal="true" aria-labelledby="advanced-title">
      <div className="sheet-heading"><div><p className="section-kicker">{t("conversionSettings")}</p><h2 id="advanced-title">{t("advancedSettings")}</h2></div><button className="icon-button neutral" type="button" aria-label={t("close")} onClick={onClose}><X size={18} aria-hidden="true" /></button></div>
      <div className="option-grid">
        <label><span>{t("formatHint")}</span><span className="select-shell"><select value={value.format ?? ""} onChange={(event) => patch("format", (event.target.value || null) as InputFormat | null)}><option value="">{t("automatic")}</option>{FORMATS.map((format) => <option key={format} value={format}>{format}</option>)}</select><ChevronDown size={15} aria-hidden="true" /></span></label>
        <label><span>{t("ocrConfidence")}</span><input type="number" min="0" max="1" step="0.05" value={value.ocrConfidence} onChange={(event) => patch("ocrConfidence", Number(event.target.value))} /></label>
        <label><span>{t("aiMode")}</span><span className="select-shell"><select value={value.aiMode} onChange={(event) => patch("aiMode", event.target.value as AiMode)}><option value="off">{t("localOnly")}</option><option value="fallback">{t("aiWhenLocalFails")}</option><option value="prefer">{t("preferAi")}</option><option value="only">{t("aiOnly")}</option></select><ChevronDown size={15} aria-hidden="true" /></span></label>
        <label><span>{t("maxInput")}</span><input type="number" min="1" max="512" value={value.maxInputMiB} onChange={(event) => patch("maxInputMiB", Number(event.target.value))} /></label>
        <label><span>{t("maxMemory")}</span><input type="number" min="1" max="2048" value={value.maxMemoryMiB} onChange={(event) => patch("maxMemoryMiB", Number(event.target.value))} /></label>
        <label><span>{t("maxPages")}</span><input type="number" min="1" max="10000" value={value.maxPages} onChange={(event) => patch("maxPages", Number(event.target.value))} /></label>
      </div>
      <div className="authorization-box">
        <div className="network-choice"><div className="network-icon">{value.networkMode === "unrestricted" ? <Wifi size={18} aria-hidden="true" /> : <WifiOff size={18} aria-hidden="true" />}</div><div><strong>{t("networkAccess")}</strong><p>{t(value.networkMode === "unrestricted" ? "networkEnabledNote" : "networkDisabledNote")}</p></div><label className="switch"><span className="visually-hidden">{t("networkAccess")}</span><input type="checkbox" checked={value.networkMode === "unrestricted"} onChange={(event) => patch("networkMode", (event.target.checked ? "unrestricted" : "restricted") as NetworkMode)} /><span aria-hidden="true" /></label></div>
        {(value.aiMode !== "off" || providerCapabilityActive) && <label className="check grant"><input type="checkbox" checked={value.authorizeProvider} onChange={(event) => patch("authorizeProvider", event.target.checked)} />{t("authorizeProvider")}</label>}
      </div>
    </aside>
  </div>;
}

export function AudioSetupDialog({ status, open, onClose, onInstall }: { status?: ComponentStatus | undefined; open: boolean; onClose(): void; onInstall(): Promise<void> }) {
  const { t } = useI18n();
  const [installing, setInstalling] = useState(false);
  const [error, setError] = useState(false);
  if (!open) return null;
  const install = async () => {
    setInstalling(true); setError(false);
    try { await onInstall(); onClose(); } catch { setError(true); } finally { setInstalling(false); }
  };
  return <div className="sheet-backdrop modal-backdrop" role="presentation" onMouseDown={(event) => { if (event.currentTarget === event.target) onClose(); }}>
    <section className="setup-dialog" role="dialog" aria-modal="true" aria-labelledby="audio-setup-title">
      <div className="sheet-heading"><div><p className="section-kicker">{t("audioTranscription")}</p><h2 id="audio-setup-title">{t("prepareAudioTitle")}</h2></div><button className="icon-button neutral" type="button" aria-label={t("close")} onClick={onClose}><X size={18} aria-hidden="true" /></button></div>
      <div className="source-choice-list">
        <button className="source-choice" type="button" onClick={() => void install()} disabled={installing}>
          <span><strong>{t("installLocalSpeech")}</strong><small>{t("runsOnThisDevice")}</small></span>
          {installing ? <LoaderCircle className="spin" size={18} aria-hidden="true" /> : <span aria-hidden="true">→</span>}
        </button>
        <RouteLink className="source-choice" href="/admin/capabilities">
          <span><strong>{t("chooseAiService")}</strong><small>{t("usesNetwork")}</small></span><span aria-hidden="true">→</span>
        </RouteLink>
      </div>
      {status?.detail && <p className="runtime-detail"><CircleAlert size={16} aria-hidden="true" />{t("speechSourceUnavailable")}</p>}
      {error && <p className="runtime-detail" role="status"><CircleAlert size={16} aria-hidden="true" />{t("installComponentsFailed")}</p>}
      <div className="dialog-actions"><button className="secondary" type="button" onClick={onClose} disabled={installing}>{t("close")}</button></div>
    </section>
  </div>;
}
