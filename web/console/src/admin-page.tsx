import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import {
  Activity, CheckCircle2, CircleAlert, Cloud, Download, FileSearch,
  Package, Plus, Search, Settings2, ShieldCheck, Speech, ScanText, FileType2, Trash2, Wrench, X,
} from "lucide-react";
import type {
  AdminAction, AdminSnapshot, ApiClient, DoctorAdmin,
  CapabilityAdmin, FormatAdmin, PluginAdmin, ProviderAdmin,
} from "./api";
import { useI18n, type Locale } from "./i18n";
import { RouteLink } from "./router";

export type AdminSection = "capabilities" | "formats" | "providers" | "plugins" | "configuration" | "doctor";
export const adminSections: AdminSection[] = ["capabilities", "configuration", "doctor"];
const adminSnapshotCache = new WeakMap<ApiClient, Map<AdminSection, AdminSnapshot>>();
function cachedSnapshot(api: ApiClient, section: AdminSection) { return adminSnapshotCache.get(api)?.get(section); }
function cacheSnapshot(api: ApiClient, section: AdminSection, snapshot: AdminSnapshot) { const cache = adminSnapshotCache.get(api) ?? new Map<AdminSection, AdminSnapshot>(); cache.set(section, snapshot); adminSnapshotCache.set(api, cache); }

interface ActionOptions { dangerous?: boolean; network?: boolean; confirm?: string; success: string }
interface ActionFeedback { target: string; kind: "error" | "success"; message: string }

function useDialogLifecycle<T extends HTMLElement>(open: boolean, close: () => void) {
  const dialogRef = useRef<T>(null);
  const closeRef = useRef(close);
  closeRef.current = close;
  useEffect(() => {
    if (!open) return;
    const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const frame = window.requestAnimationFrame(() => {
      dialogRef.current?.querySelector<HTMLElement>("button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), [tabindex]:not([tabindex='-1'])")?.focus();
    });
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Escape" || !dialogRef.current) return;
      const dialogs = document.querySelectorAll('[role="dialog"]');
      if (dialogs[dialogs.length - 1] !== dialogRef.current) return;
      event.preventDefault();
      event.stopImmediatePropagation();
      closeRef.current();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => {
      window.cancelAnimationFrame(frame);
      window.removeEventListener("keydown", onKeyDown);
      if (previous?.isConnected) window.requestAnimationFrame(() => previous.focus());
    };
  }, [open]);
  return dialogRef;
}
interface SectionProps {
  api: ApiClient;
  snapshot: AdminSnapshot;
  busy: boolean;
  locale: Locale;
  act: (action: AdminAction, options: ActionOptions) => Promise<boolean>;
  feedback?: ActionFeedback | null;
}

const zh = {
  heading: "系统管理", intro: "查看转换能力、连接 AI 服务，并处理本机运行问题。",
  tabs: { capabilities: "能力与来源", formats: "格式支持", providers: "AI 服务", plugins: "扩展插件", configuration: "偏好设置", doctor: "运行诊断" },
  loading: "正在读取本机状态…", retry: "重新加载", viewOnly: "当前只能查看", viewOnlyBody: "本次启动未开放修改权限。你仍可以查看状态和运行只读检查。",
  success: "操作已完成", advanced: "安装校验", technical: "详情", cancel: "取消", add: "添加", edit: "编辑", save: "保存", remove: "卸载", verify: "验证", install: "安装", test: "测试连接", setDefault: "设为默认", enable: "启用", disable: "停用", show: "查看", path: "查看位置", restore: "恢复默认", create: "创建", copy: "复制自",
  formatsTitle: "识别文件格式", formatsBody: "输入一个本地文件路径，确认它能否被转换。", localPath: "文件路径", localPathHint: "例如 /path/to/report.pdf", detect: "开始识别", charset: "文本编码", formatHint: "指定格式", extension: "文件扩展名", mime: "MIME 类型", hosts: "允许访问的主机", privateNetwork: "允许访问局域网地址", formatLibrary: "支持的格式", formatSearch: "搜索格式或扩展名", allFormats: "全部格式", needsRuntime: "需要额外组件", ready: "可用", unavailable: "不可用", source: "来源", extensions: "扩展名", runtime: "所需组件",
  capabilitiesTitle: "能力与来源", capabilitiesBody: "", installed: "已安装", notInstalled: "未安装", defaultModel: "默认 AI 服务", downloadOptions: "下载选项", insecure: "允许使用不安全的 HTTP 下载", capabilityRemoveConfirm: "确定卸载这项本地能力吗？", currentSource: "当前使用", localPlugin: "本地", remoteProvider: "AI 服务", off: "关闭", officeCapability: "旧版 Office", officeCapabilityBody: "转换 .doc、.xls 和 .ppt", ocrCapability: "图片 OCR", ocrCapabilityBody: "识别扫描 PDF 和图片", speechCapability: "语音", speechCapabilityBody: "语音转写与说话人识别", transcription: "语音转写", diarization: "说话人识别", useSource: "切换", repair: "修复", capabilityInstallSuccess: "已安装", capabilitySourceSuccess: "来源已更新", capabilityVerifySuccess: "验证通过", capabilityRemoveSuccess: "已卸载", localOnly: "本地",
  providersTitle: "AI 服务", providersBody: "", noProviders: "尚未连接 AI 服务", noProvidersBody: "可按需连接一个 AI 服务。", addProvider: "连接 AI 服务", serviceName: "服务名称", baseUrl: "API 地址", model: "默认模型", ocrModel: "OCR 模型", transcriptionModel: "转写模型", modelMappingHint: "未指定时使用默认模型。", apiKeyEnv: "密钥环境变量", apiKeyHint: "填写保存密钥的环境变量名称。", capabilities: "支持的能力", timeout: "超时时间（毫秒）", scope: "保存位置", project: "当前项目", global: "所有项目", environmentReady: "密钥已就绪", environmentMissing: "未找到密钥", inherited: "沿用上层设置", effective: "当前生效", overridden: "已被覆盖", providerRemoveConfirm: "确定删除这个 AI 服务吗？", providerDefaultSuccess: "已设为默认 AI 服务", providerAddedSuccess: "AI 服务已添加", providerTestSuccess: "连接测试已完成",
  pluginsTitle: "本地扩展", pluginsBody: "", noPlugins: "尚未安装本地扩展", noPluginsBody: "", addPlugin: "安装扩展", packageSource: "安装来源", sha: "SHA-256", signer: "签名方", fingerprint: "签名指纹", pluginRemoveConfirm: "确定卸载这个本地扩展吗？", pluginInstallSuccess: "本地扩展已安装", pluginVerifySuccess: "验证完成", enabled: "已启用", disabled: "已停用", version: "版本", target: "适用平台", protocol: "协议", verification: "验证状态",
  configTitle: "偏好设置", configBody: "", chooseSetting: "选择设置", value: "设置值", readCurrent: "读取当前值", promptName: "提示词名称", addPrompt: "选择提示词设置", profiles: "设置方案", newProfile: "新方案名称", copyFrom: "复制已有方案", noProfiles: "还没有自定义设置方案。", configTools: "配置文件", configToolsBody: "", validationPath: "配置文件", resolved: "显示最终值", force: "覆盖已有配置文件", paths: "配置文件位置", validate: "验证配置", initialize: "初始化配置", rawConfig: "完整配置", configSaved: "设置已保存", configRestored: "已恢复默认设置", profileCreated: "设置方案已创建", profileRemoveConfirm: "确定删除这个设置方案吗？",
  doctorTitle: "运行诊断", doctorBody: "检查本机运行环境，并给出可以直接执行的处理建议。", run: "重新检查", checkNetwork: "同时检查联网能力", healthy: "没有发现问题", healthyBody: "当前检查项目均正常。", attention: "项需要处理", passed: "项正常", notRun: "项未检查", passedChecks: "查看正常项目", skippedChecks: "查看未检查项目", impact: "影响", nextStep: "处理建议", doctorDone: "诊断已完成",
  detectionResult: "识别结果", confidence: "匹配度", providerResult: "连接结果", availableModels: "可用模型", result: "操作结果", rawResult: "查看原始结果",
};
const en: typeof zh = {
  heading: "System management", intro: "Check conversion capabilities, connect AI services, and resolve local runtime issues.",
  tabs: { capabilities: "Capabilities & sources", formats: "Format support", providers: "AI services", plugins: "Extensions", configuration: "Preferences", doctor: "Diagnostics" },
  loading: "Reading local status…", retry: "Reload", viewOnly: "View-only mode", viewOnlyBody: "This launch does not allow configuration changes. You can still inspect status and run read-only checks.",
  success: "Done", advanced: "Installation checks", technical: "Details", cancel: "Cancel", add: "Add", edit: "Edit", save: "Save", remove: "Uninstall", verify: "Verify", install: "Install", test: "Test connection", setDefault: "Set as default", enable: "Enable", disable: "Disable", show: "Show", path: "Show location", restore: "Restore default", create: "Create", copy: "Copy from",
  formatsTitle: "Identify a file format", formatsBody: "Enter a local file path to confirm whether it can be converted.", localPath: "File path", localPathHint: "For example, /path/to/report.pdf", detect: "Identify format", charset: "Text encoding", formatHint: "Specify format", extension: "File extension", mime: "MIME type", hosts: "Allowed hosts", privateNetwork: "Allow local-network addresses", formatLibrary: "Supported formats", formatSearch: "Search formats or extensions", allFormats: "All formats", needsRuntime: "Additional component required", ready: "Available", unavailable: "Unavailable", source: "Source", extensions: "Extensions", runtime: "Required component",
  capabilitiesTitle: "Capabilities & sources", capabilitiesBody: "", installed: "Installed", notInstalled: "Not installed", defaultModel: "Default AI service", downloadOptions: "Download options", insecure: "Allow insecure HTTP downloads", capabilityRemoveConfirm: "Uninstall this local capability?", currentSource: "In use", localPlugin: "Local", remoteProvider: "AI service", off: "Off", officeCapability: "Legacy Office", officeCapabilityBody: "Convert .doc, .xls, and .ppt", ocrCapability: "Image OCR", ocrCapabilityBody: "Read scanned PDFs and images", speechCapability: "Speech", speechCapabilityBody: "Transcription and speaker identification", transcription: "Speech transcription", diarization: "Speaker identification", useSource: "Switch", repair: "Repair", capabilityInstallSuccess: "Installed", capabilitySourceSuccess: "Source updated", capabilityVerifySuccess: "Verified", capabilityRemoveSuccess: "Uninstalled", localOnly: "Local",
  providersTitle: "AI services", providersBody: "", noProviders: "No AI service connected", noProvidersBody: "Connect one when needed.", addProvider: "Connect AI service", serviceName: "Service name", baseUrl: "API address", model: "Default model", ocrModel: "OCR model", transcriptionModel: "Transcription model", modelMappingHint: "Uses the default model when blank.", apiKeyEnv: "API key environment variable", apiKeyHint: "Enter the environment variable that holds the key.", capabilities: "Capabilities", timeout: "Timeout (milliseconds)", scope: "Save for", project: "This project", global: "All projects", environmentReady: "API key ready", environmentMissing: "API key not found", inherited: "Inherited", effective: "Active", overridden: "Overridden", providerRemoveConfirm: "Remove this AI service?", providerDefaultSuccess: "Default AI service updated", providerAddedSuccess: "AI service added", providerTestSuccess: "Connection test completed",
  pluginsTitle: "Local extensions", pluginsBody: "", noPlugins: "No local extensions installed", noPluginsBody: "", addPlugin: "Install extension", packageSource: "Package path or HTTPS URL", sha: "File checksum (SHA-256)", signer: "Signer ID", fingerprint: "Signing fingerprint", pluginRemoveConfirm: "Uninstall this local extension?", pluginInstallSuccess: "Extension installed", pluginVerifySuccess: "Verification completed", enabled: "Enabled", disabled: "Disabled", version: "Version", target: "Platform", protocol: "Protocol", verification: "Verification",
  configTitle: "Preferences", configBody: "", chooseSetting: "Choose a setting", value: "Value", readCurrent: "Read current value", promptName: "Prompt name", addPrompt: "Select prompt setting", profiles: "Setting profiles", newProfile: "New profile name", copyFrom: "Copy an existing profile", noProfiles: "No custom profiles yet.", configTools: "Configuration file", configToolsBody: "", validationPath: "Configuration file", resolved: "Show resolved values", force: "Overwrite existing file", paths: "Configuration locations", validate: "Validate configuration", initialize: "Initialize configuration", rawConfig: "Full configuration", configSaved: "Setting saved", configRestored: "Default restored", profileCreated: "Profile created", profileRemoveConfirm: "Remove this setting profile?",
  doctorTitle: "Diagnostics", doctorBody: "Check the local runtime and get concrete steps for anything that needs attention.", run: "Run again", checkNetwork: "Also check network access", healthy: "No issues found", healthyBody: "All current checks passed.", attention: "need attention", passed: "passed", notRun: "not checked", passedChecks: "View passing checks", skippedChecks: "View checks not run", impact: "Impact", nextStep: "What to do", doctorDone: "Diagnostics completed",
  detectionResult: "Detection result", confidence: "Confidence", providerResult: "Connection result", availableModels: "Available models", result: "Operation result", rawResult: "View raw result",
};
function copy(locale: Locale) { return locale === "zh-CN" ? zh : en; }

export function AdminPage({ api, section, initialContext }: { api: ApiClient; section: AdminSection; initialContext?: "formats" | "providers" | "plugins" }) {
  const { locale } = useI18n();
  const c = copy(locale);
  const [snapshot, setSnapshot] = useState<AdminSnapshot | null>(() => cachedSnapshot(api, section) ?? null);
  const [error, setError] = useState("");
  const [actionFeedback, setActionFeedback] = useState<ActionFeedback | null>(null);
  const [attempt, setAttempt] = useState(0);
  const [busy, setBusy] = useState(false);
  const actionInFlight = useRef(false);

  useEffect(() => { setActionFeedback(null); setSnapshot(cachedSnapshot(api, section) ?? null); }, [api, section]);
  useEffect(() => {
    const controller = new AbortController();
    setError("");
    void api.admin(controller.signal, section).then((next) => { cacheSnapshot(api, section, next); setSnapshot(next); }, (reason: unknown) => {
      if (!controller.signal.aborted) setError(errorCode(reason));
    });
    return () => controller.abort();
  }, [api, attempt, section]);
  const refreshAdmin = async () => { const next = await api.admin(undefined, section); cacheSnapshot(api, section, next); setSnapshot(next); };

  const act = async (action: AdminAction, options: ActionOptions) => {
    if (actionInFlight.current || options.confirm && !window.confirm(options.confirm)) return false;
    const actionTrigger = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    actionInFlight.current = true;
    setBusy(true); setActionFeedback(null);
    const feedbackTarget = action.target ?? "admin";
    let restoreCapabilityFocus = false;
    const requested = { ...action, schemaVersion: 1 as const, authorizeDangerous: options.dangerous === true, authorizeNetwork: options.network === true };
    try {
      const authorizationGrant = options.dangerous || options.network ? await api.adminGrant(requested) : undefined;
      const outcome = await api.adminAction({ ...requested, ...(authorizationGrant ? { authorizationGrant } : {}) });
      const operationResult = outcome.operationResult;
      if (operationResult) setSnapshot((current) => current ? { ...current, operationResult } : current);
      else await refreshAdmin();
      setActionFeedback({ target: feedbackTarget, kind: "success", message: options.success });
      return true;
    } catch (reason) {
      const code = errorCode(reason);
      setActionFeedback({ target: feedbackTarget, kind: "error", message: friendlyError(code, locale) });
      restoreCapabilityFocus = true;
      return false;
    }
    finally {
      actionInFlight.current = false;
      setBusy(false);
      if (restoreCapabilityFocus) {
        window.requestAnimationFrame(() => window.requestAnimationFrame(() => {
          if (actionTrigger?.isConnected && !actionTrigger.matches(":disabled")) actionTrigger.focus();
          else document.querySelector<HTMLElement>(`[data-capability-target="${CSS.escape(feedbackTarget)}"]:not(:disabled)`)?.focus();
        }));
      }
    }
  };

  return <section className="admin-page">
    <div className="admin-shell"><nav className="admin-tabs" aria-label={c.heading}>
      {adminSections.map((item) => <RouteLink key={item} href={`/admin/${item}`} className={section === item ? "active" : ""}>{adminNavIcon(item)}<span>{c.tabs[item]}</span></RouteLink>)}
    </nav>
    <div className="admin-content"><h1 className="visually-hidden">{c.tabs[section]}</h1>{error && <Feedback kind="error"><strong>{friendlyError(error, locale)}</strong><button type="button" className="secondary" onClick={() => setAttempt((value) => value + 1)}>{c.retry}</button></Feedback>}
    {!snapshot && !error ? section === "configuration" ? <PreferencesSkeleton locale={locale} /> : <div className="admin-loading" role="status"><Activity className="spinner" size={18} /><span>{c.loading}</span></div> : snapshot ? <>
      {snapshot.configurationReadOnly && (section === "capabilities" || section === "providers" || section === "plugins" || section === "configuration") && <Feedback kind="readonly"><ShieldCheck size={20} /><div><strong>{c.viewOnly}</strong><p>{c.viewOnlyBody}</p></div></Feedback>}
      {section === "capabilities" && <CapabilitiesSection api={api} refreshAdmin={refreshAdmin} snapshot={snapshot} busy={busy} locale={locale} act={act} feedback={actionFeedback} {...(initialContext ? { initialContext } : {})} />}
      {section === "configuration" && <ConfigurationSection api={api} snapshot={snapshot} busy={busy} locale={locale} act={act} feedback={actionFeedback} />}
      {section === "doctor" && <DoctorSection api={api} snapshot={snapshot} busy={busy} locale={locale} act={act} feedback={actionFeedback} />}
    </> : null}</div></div>
  </section>;
}

function adminNavIcon(section: AdminSection) {
  if (section === "capabilities") return <Settings2 size={17} />;
  if (section === "formats") return <FileType2 size={17} />;
  if (section === "providers") return <Cloud size={17} />;
  if (section === "plugins") return <Package size={17} />;
  if (section === "configuration") return <Settings2 size={17} />;
  return <Activity size={17} />;
}

function FormatsSection({ snapshot, locale }: SectionProps) {
  const c = copy(locale);
  const [search, setSearch] = useState(""); const [page, setPage] = useState(0);
  const formats = useMemo(() => snapshot.formats.filter((item) => `${item.format} ${item.family} ${item.extensions.join(" ")}`.toLowerCase().includes(search.toLowerCase().trim())), [snapshot.formats, search]);
  const visibleFormats = formats.slice(page * 8, page * 8 + 8);
  useEffect(() => { if (page > Math.max(0, Math.ceil(formats.length / 8) - 1)) setPage(0); }, [formats.length, page]);
  return <div className="admin-section-stack">
    <SectionTitle icon={<FileSearch />} title={c.formatLibrary} body={locale === "zh-CN" ? "查看当前可转换的文件类型与所需组件。" : "See which file types are available and which components they need."} />
    <div className="admin-list-heading"><div><h2>{c.formatLibrary}</h2><p>{snapshot.formats.length} {c.allFormats.toLowerCase()}</p></div><label className="admin-search"><Search size={16} /><span className="sr-only">{c.formatSearch}</span><input placeholder={c.formatSearch} value={search} onChange={(event) => { setSearch(event.target.value); setPage(0); }} /></label></div>
    <div className="card admin-table-shell"><div className="admin-table-scroll"><table className="admin-table"><thead><tr><th>{locale === "zh-CN" ? "格式" : "Format"}</th><th>{locale === "zh-CN" ? "类型" : "Type"}</th><th>{c.extensions}</th><th>{c.runtime}</th><th>{locale === "zh-CN" ? "状态" : "Status"}</th></tr></thead><tbody>{visibleFormats.map((item) => <FormatRow key={item.format} item={item} locale={locale} />)}</tbody></table></div><PageControls page={page} total={formats.length} pageSize={8} setPage={setPage} locale={locale} /></div>
  </div>;
}

function FormatRow({ item, locale }: { item: FormatAdmin; locale: Locale }) {
  const c = copy(locale); const ready = item.status === "supported" || item.status === "available" || !item.runtimeComponent;
  return <tr><td><strong>{item.format.toUpperCase()}</strong></td><td>{friendlyFamily(item.family, locale)}</td><td>{item.extensions.join("、")}</td><td>{item.runtimeComponent ? pluginDisplayName(item.runtimeComponent, locale) : locale === "zh-CN" ? "内置" : "Built in"}</td><td><StatusBadge tone={ready ? "ok" : "warning"}>{ready ? c.ready : c.needsRuntime}</StatusBadge></td></tr>;
}

function CapabilitiesSection({ api, snapshot, busy, locale, act, feedback, refreshAdmin, initialContext }: SectionProps & { refreshAdmin: () => Promise<void>; initialContext?: "formats" | "providers" | "plugins" }) {
  const c = copy(locale);
  const [sourceManager, setSourceManager] = useState<"formats" | "providers" | "plugins" | null>(initialContext ?? null);
  const sourceManagerRef = useDialogLifecycle<HTMLElement>(Boolean(sourceManager), () => setSourceManager(null));
  const find = (id: CapabilityAdmin["id"]) => snapshot.capabilities.find((item) => item.id === id)!;
  return <div className="admin-section-stack"><SectionTitle icon={<Settings2 />} title={c.capabilitiesTitle} body={c.capabilitiesBody} action={<div className="source-manager-actions"><button className="secondary" type="button" onClick={() => setSourceManager("providers")}><Cloud size={17} />{c.providersTitle}</button><button className="secondary" type="button" onClick={() => setSourceManager("plugins")}><Package size={17} />{c.pluginsTitle}</button></div>} />
    <div className="admin-grid capability-grid">
      <CapabilityCard api={api} refreshAdmin={refreshAdmin} item={find("legacy-office")} title={c.officeCapability} body={c.officeCapabilityBody} icon={<FileType2 size={18} />} busy={busy} locale={locale} readOnly={snapshot.configurationReadOnly} act={act} feedback={feedback?.target === "legacy-office" ? feedback : null} />
      <CapabilityCard api={api} refreshAdmin={refreshAdmin} item={find("ocr")} title={c.ocrCapability} body={c.ocrCapabilityBody} icon={<ScanText size={18} />} busy={busy} locale={locale} readOnly={snapshot.configurationReadOnly} act={act} feedback={feedback?.target === "ocr" ? feedback : null} />
      <CapabilityCard api={api} refreshAdmin={refreshAdmin} item={find("transcription")} title={c.transcription} body={locale === "zh-CN" ? "将音频和视频转换为带时间的逐字稿" : "Turn audio and video into timestamped transcripts"} icon={<Speech size={18} />} busy={busy} locale={locale} readOnly={snapshot.configurationReadOnly} act={act} feedback={feedback && ["transcription", "media"].includes(feedback.target) ? feedback : null} removeTarget="media" />
      <CapabilityCard api={api} refreshAdmin={refreshAdmin} item={find("diarization")} title={c.diarization} body={locale === "zh-CN" ? "在逐字稿中区分不同发言人" : "Separate speakers in a transcript"} icon={<Speech size={18} />} busy={busy} locale={locale} readOnly={snapshot.configurationReadOnly} act={act} feedback={feedback?.target === "diarization" ? feedback : null} manageLocal={false} removeTarget="media" />
    </div>
    {sourceManager && <div className="sheet-backdrop modal-backdrop" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget) setSourceManager(null); }}><article ref={sourceManagerRef} className="setup-dialog admin-dialog source-manager-dialog" role="dialog" aria-modal="true" aria-labelledby="source-manager-title"><div className="drawer-heading"><h2 id="source-manager-title">{sourceManager === "providers" ? c.providersTitle : sourceManager === "plugins" ? c.pluginsTitle : c.tabs.formats}</h2><button className="icon-button neutral" type="button" aria-label={c.cancel} onClick={() => setSourceManager(null)}><X size={19} /></button></div>{sourceManager === "providers" ? <ProvidersSection api={api} snapshot={snapshot} busy={busy} locale={locale} act={act} feedback={feedback ?? null} /> : sourceManager === "plugins" ? <PluginsSection api={api} snapshot={snapshot} busy={busy} locale={locale} act={act} feedback={feedback ?? null} /> : <FormatsSection api={api} snapshot={snapshot} busy={busy} locale={locale} act={act} />}</article></div>}
  </div>;
}

function CapabilityCard({ api, refreshAdmin, item, title, body, icon, busy, locale, readOnly, act, feedback, manageLocal = true }: { api: ApiClient; refreshAdmin: () => Promise<void>; item: CapabilityAdmin; title: string; body: string; icon: ReactNode; busy: boolean; locale: Locale; readOnly: boolean; act: SectionProps["act"]; feedback?: ActionFeedback | null; manageLocal?: boolean; removeTarget?: string }) {
  const ready = item.status === "ready";
  return <article className="card admin-entity-card capability-row"><div className="entity-card-heading"><div className="entity-icon">{icon}</div><div><h3>{title}</h3><p>{body}</p></div><StatusBadge tone={ready ? "ok" : item.status === "corrupt" ? "warning" : "neutral"}>{friendlyCapabilityStatus(item.status, locale)}</StatusBadge></div>
    <CapabilityControl api={api} refreshAdmin={refreshAdmin} item={item} manageLocal={manageLocal} busy={busy} locale={locale} readOnly={readOnly} act={act} feedback={feedback ?? null} />
  </article>;
}

function CapabilityControl({ api, refreshAdmin, item, label, manageLocal = true, busy, locale, readOnly, act, feedback }: { api: ApiClient; refreshAdmin: () => Promise<void>; item: CapabilityAdmin; label?: string; manageLocal?: boolean; busy: boolean; locale: Locale; readOnly: boolean; act: SectionProps["act"]; feedback?: ActionFeedback | null }) {
  const c = copy(locale); const [source, setSource] = useState(item.currentSource); const localReady = item.localStatus === "ready"; const needsRepair = item.localStatus === "corrupt" || item.localStatus === "incompatible";
  useEffect(() => setSource(item.currentSource), [item.currentSource]);
  return <div className="capability-control">{label && <h4>{label}</h4>}<Field label={c.currentSource}><select value={source} disabled={busy || readOnly} onChange={(event) => setSource(event.target.value)}>{item.sources.map((value) => <option key={value} value={value} disabled={value.startsWith("plugin:") && item.localStatus === "not-installed"}>{friendlyCapabilitySource(value, locale)}</option>)}</select></Field>
    <div className="admin-form-actions"><button data-capability-target={item.id} disabled={busy || readOnly || source === item.currentSource} type="button" onClick={() => void act({ schemaVersion: 1, action: "capability.use", scope: "project", target: item.id, source }, { dangerous: true, success: c.capabilitySourceSuccess })}>{c.useSource}</button>{manageLocal && (localReady ? <CapabilityVerifyButton api={api} capability={item.id} locale={locale} disabled={busy || readOnly} fallback={() => act({ schemaVersion: 1, action: "capability.verify", target: item.id }, { success: c.capabilityVerifySuccess })} onComplete={refreshAdmin} /> : <button data-capability-target={item.id} disabled={busy || readOnly} type="button" onClick={() => void act({ schemaVersion: 1, action: "capability.install", target: item.id }, { dangerous: true, network: true, success: c.capabilityInstallSuccess })}><Download size={17} />{needsRepair ? c.repair : c.install}</button>)}</div>
    {feedback && <p className={`capability-feedback ${feedback.kind}`} role={feedback.kind === "error" ? "alert" : "status"} aria-live={feedback.kind === "error" ? "assertive" : "polite"}>{feedback.kind === "success" && <CheckCircle2 size={16} />}{feedback.message}</p>}
  </div>;
}

function CapabilityVerifyButton({ api, capability, locale, disabled, fallback, onComplete }: { api: ApiClient; capability: CapabilityAdmin["id"]; locale: Locale; disabled: boolean; fallback: () => Promise<unknown>; onComplete: () => Promise<void> }) {
  const c = copy(locale); const [check, setCheck] = useState<import("./api").CapabilityCheck | null>(null); const [error, setError] = useState("");
  const running = check && ["queued", "running", "cancelling"].includes(check.status);
  const start = async () => {
    setError("");
    if (!api.startCapabilityCheck || !api.capabilityCheck) { await fallback(); return; }
    try {
      let next = await api.startCapabilityCheck(capability); setCheck(next);
      while (["queued", "running", "cancelling"].includes(next.status)) {
        await new Promise((resolve) => window.setTimeout(resolve, 350));
        next = await api.capabilityCheck(next.id); setCheck(next);
      }
      if (next.status === "completed") { await onComplete(); window.dispatchEvent(new Event("into-md-capabilities-changed")); }
      else if (next.status === "failed") setError(friendlyError(next.code ?? "requestFailed", locale));
    } catch (reason) { setError(friendlyError(errorCode(reason), locale)); }
  };
  return <div className="capability-verify"><button className="secondary" data-capability-target={capability} disabled={disabled || Boolean(running)} type="button" onClick={() => void start()}>{running ? <Activity className="spinner" size={16} /> : null}{running ? `${check?.progress ?? 0}%` : c.verify}</button>{running && api.cancelCapabilityCheck && <button className="tertiary" type="button" onClick={() => void api.cancelCapabilityCheck?.(check!.id).then(setCheck)}>{c.cancel}</button>}{check && <p className={`capability-feedback ${check.status === "completed" ? "success" : check.status === "failed" ? "error" : "progress"}`} role={check.status === "failed" ? "alert" : "status"}>{check.status === "completed" ? c.capabilityVerifySuccess : check.status === "failed" ? error : `${friendlyCheckStage(check.stage, locale)} · ${check.progress}%`}</p>}</div>;
}

function ProvidersSection({ snapshot, busy, locale, act, feedback }: SectionProps) {
  const c = copy(locale); const [open, setOpen] = useState(false); const [editing, setEditing] = useState(false); const [name, setName] = useState(""); const [url, setUrl] = useState(""); const [model, setModel] = useState(""); const [ocrModel, setOcrModel] = useState(""); const [transcriptionModel, setTranscriptionModel] = useState(""); const [env, setEnv] = useState(""); const [capabilities, setCapabilities] = useState(""); const [timeout, setTimeoutValue] = useState(""); const [scope, setScope] = useState<"global" | "project">("project"); const [hosts, setHosts] = useState(""); const [privateNetwork, setPrivateNetwork] = useState(false); const [page, setPage] = useState(0);
  const effective = snapshot.providers.filter((item) => item.effective); const validTimeout = timeout === "" || /^[1-9][0-9]{0,7}$/.test(timeout) && Number(timeout) <= 86_400_000;
  const visible = effective.slice(page * 6, page * 6 + 6);
  const providerCapabilities = [...new Set([...csv(capabilities), ...(ocrModel ? ["vision-ocr"] : []), ...(transcriptionModel ? ["audio-transcription"] : [])])];
  const dialogRef = useDialogLifecycle<HTMLElement>(open, () => setOpen(false));
  const clear = () => { setEditing(false); setName(""); setUrl(""); setModel(""); setOcrModel(""); setTranscriptionModel(""); setEnv(""); setCapabilities(""); setTimeoutValue(""); setScope("project"); setHosts(""); setPrivateNetwork(false); };
  const add = () => { clear(); setOpen(true); };
  const edit = (item: ProviderAdmin) => { setEditing(true); setName(item.name); setUrl(item.baseUrl ?? ""); setModel(item.model ?? ""); setOcrModel(item.models["vision-ocr"] ?? item.models.ocr ?? ""); setTranscriptionModel(item.models["audio-transcription"] ?? item.models.transcription ?? ""); setEnv(item.apiKeyEnv ?? ""); setCapabilities(item.capabilities.filter((value) => !["vision-ocr", "ocr", "audio-transcription", "transcription"].includes(value)).join(", ")); setTimeoutValue(item.timeoutMs ? String(item.timeoutMs) : ""); setScope(item.actionScope ?? "project"); setHosts(item.allowedHosts.join(", ")); setPrivateNetwork(item.allowPrivateNetwork); setOpen(true); };
  return <div className="admin-section-stack admin-source-section"><SectionTitle icon={<Cloud />} title={c.providersTitle} body={c.providersBody} action={<button type="button" disabled={snapshot.configurationReadOnly} onClick={add}><Plus size={17} />{c.addProvider}</button>} />
    {open && <div className="sheet-backdrop modal-backdrop" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget) setOpen(false); }}><article ref={dialogRef} className="setup-dialog admin-dialog" role="dialog" aria-modal="true" aria-labelledby="provider-dialog-title"><div className="drawer-heading"><h2 id="provider-dialog-title">{editing ? `${c.edit} ${name}` : c.addProvider}</h2><button className="icon-button neutral" type="button" aria-label={c.cancel} onClick={() => setOpen(false)}><X size={19} /></button></div><div className="admin-form-grid"><Field label={c.serviceName}><input value={name} disabled={editing} maxLength={128} onChange={(event) => setName(event.target.value)} /></Field><Field label={c.baseUrl}><input value={url} maxLength={4096} placeholder="https://api.example.com/v1" onChange={(event) => setUrl(event.target.value)} /></Field><Field label={c.apiKeyEnv}><input value={env} maxLength={128} placeholder="AI_SERVICE_API_KEY" onChange={(event) => setEnv(event.target.value)} /></Field><Field label={c.model}><input value={model} maxLength={512} onChange={(event) => setModel(event.target.value)} /></Field><Field label={c.ocrModel}><input value={ocrModel} maxLength={512} onChange={(event) => setOcrModel(event.target.value)} /></Field><Field label={c.transcriptionModel}><input value={transcriptionModel} maxLength={512} onChange={(event) => setTranscriptionModel(event.target.value)} /></Field></div>
      <div className="admin-dialog-section"><h3>{locale === "zh-CN" ? "连接选项" : "Connection options"}</h3><div className="admin-form-grid"><Field label={c.timeout}><input value={timeout} inputMode="numeric" maxLength={8} aria-invalid={!validTimeout} onChange={(event) => setTimeoutValue(event.target.value)} /></Field><Field label={c.scope}><ScopeSelect value={scope} onChange={setScope} locale={locale} /></Field><Field label={c.hosts}><input value={hosts} maxLength={4096} onChange={(event) => setHosts(event.target.value)} /></Field><CheckField label={c.privateNetwork} checked={privateNetwork} setChecked={setPrivateNetwork} /></div></div>
      <div className="admin-form-actions"><button className="secondary" type="button" onClick={() => setOpen(false)}>{c.cancel}</button><button disabled={busy || !name || !url || !model || !env || !validTimeout} type="button" onClick={() => void act({ schemaVersion: 1, action: "provider.add", scope, target: name, source: url, providerType: "openai-compatible", model, models: { ...(ocrModel ? { "vision-ocr": ocrModel } : {}), ...(transcriptionModel ? { "audio-transcription": transcriptionModel } : {}) }, apiKeyEnv: env, capabilities: providerCapabilities, allowHosts: csv(hosts), allowPrivateNetwork: privateNetwork, ...(timeout ? { timeoutMs: Number(timeout) } : {}) }, { dangerous: true, success: c.providerAddedSuccess }).then((ok) => { if (ok) setOpen(false); })}>{editing ? c.save : c.add}</button></div></article></div>}
    {effective.length === 0 ? <EmptyState icon={<Cloud />} title={c.noProviders} body={c.noProvidersBody} /> : <><div className="admin-grid">{visible.map((item) => <ProviderCard key={`${item.scope}:${item.name}`} item={item} all={snapshot.providers} busy={busy} locale={locale} readOnly={snapshot.configurationReadOnly} act={act} onEdit={() => edit(item)} feedback={feedback?.target === item.name ? feedback : null} />)}</div><PageControls page={page} total={effective.length} pageSize={6} setPage={setPage} locale={locale} /></>}
  </div>;
}

function ProviderCard({ item, all, busy, locale, readOnly, act, onEdit, feedback }: { item: ProviderAdmin; all: ProviderAdmin[]; busy: boolean; locale: Locale; readOnly: boolean; act: SectionProps["act"]; onEdit: () => void; feedback?: ActionFeedback | null }) {
  const c = copy(locale); const layers = all.filter((candidate) => candidate.name === item.name && candidate.scope !== "effective" && candidate.actionScope); const capabilities = [...new Set([...item.capabilities, ...Object.keys(item.models)])];
  return <article className="card admin-entity-card"><div className="entity-card-heading"><div className="entity-icon"><Cloud size={18} /></div><div><h3>{item.name}</h3><p>{item.model ?? c.inherited}</p></div>{item.default ? <StatusBadge tone="ok">{c.defaultModel}</StatusBadge> : <StatusBadge tone={item.environmentSet === false ? "warning" : "neutral"}>{item.environmentSet === false ? c.environmentMissing : c.environmentReady}</StatusBadge>}</div>
    <p className="admin-endpoint breakable">{item.baseUrl ?? c.inherited}</p><div className="chip-row">{capabilities.length === 0 ? <span className="status-pill">{locale === "zh-CN" ? "通用文本模型" : "General text"}</span> : capabilities.map((value) => <span className="status-pill" key={value}>{friendlyProviderCapability(value, locale)}</span>)}</div>
    <div className="admin-form-actions entity-card-actions"><button className="secondary" disabled={busy || readOnly || !item.actionScope} type="button" onClick={onEdit}>{c.edit}</button><button disabled={busy || readOnly || !item.actionScope} type="button" onClick={() => void act({ schemaVersion: 1, action: "provider.test", scope: item.actionScope, target: item.name }, { network: true, dangerous: item.allowPrivateNetwork, success: c.providerTestSuccess })}>{c.test}</button>{!item.default && item.actionScope && <button className="secondary" disabled={busy || readOnly} type="button" onClick={() => void act({ schemaVersion: 1, action: "provider.set-default", scope: item.actionScope, target: item.name }, { dangerous: true, success: c.providerDefaultSuccess })}>{c.setDefault}</button>}</div>
    {layers.length > 0 && <div className="provider-layer-actions">{layers.map((layer) => <div key={layer.scope}><span>{layer.scope === "global" ? c.global : c.project}</span><button className="danger secondary" disabled={busy || readOnly} type="button" onClick={() => void act({ schemaVersion: 1, action: "provider.remove", scope: layer.actionScope, target: item.name }, { dangerous: true, confirm: c.providerRemoveConfirm, success: c.success })}><Trash2 size={16} />{locale === "zh-CN" ? "删除" : "Remove"}</button></div>)}</div>}
    {feedback && <p className={`capability-feedback ${feedback.kind}`} role={feedback.kind === "error" ? "alert" : "status"}>{feedback.message}</p>}
    <details className="admin-advanced"><summary>{locale === "zh-CN" ? "查看连接信息" : "View connection information"}</summary><dl className="admin-detail-list"><Detail label={c.apiKeyEnv} value={item.apiKeyEnv ?? c.inherited} /><Detail label={c.timeout} value={item.timeoutMs ? String(item.timeoutMs) : c.inherited} />{Object.entries(item.models).map(([capability, mappedModel]) => <Detail key={capability} label={`${c.model} · ${friendlyProviderCapability(capability, locale)}`} value={mappedModel} />)}</dl></details>
  </article>;
}

function PluginsSection({ api, snapshot, busy, locale, act, feedback }: SectionProps) {
  const c = copy(locale); const [open, setOpen] = useState(false); const [source, setSource] = useState(""); const [file, setFile] = useState<File | null>(null); const [localError, setLocalError] = useState(""); const [sha, setSha] = useState(""); const [signer, setSigner] = useState(""); const [fingerprint, setFingerprint] = useState(""); const [scope, setScope] = useState<"global" | "project">("project"); const [page, setPage] = useState(0);
  const effective = snapshot.plugins.filter((item) => item.effective); const visible = effective.slice(page * 6, page * 6 + 6);
  const dialogRef = useDialogLifecycle<HTMLElement>(open, () => setOpen(false));
  const install = async () => {
    setLocalError("");
    try {
      const packageSource = file ? (api.stagePluginPackage ? (await api.stagePluginPackage(file)).source : "") : source;
      if (!packageSource) { setLocalError(locale === "zh-CN" ? "当前服务不支持从浏览器上传插件包，请使用 HTTPS 地址。" : "This service cannot upload a plugin package from the browser. Use an HTTPS URL instead."); return; }
      const ok = await act({ schemaVersion: 1, action: "plugin.install", scope, source: packageSource, ...(sha ? { sha256: sha } : {}), ...(signer ? { signingKeyId: signer } : {}), ...(fingerprint ? { signingKeySha256: fingerprint } : {}) }, { dangerous: true, network: /^https:\/\//i.test(packageSource), success: c.pluginInstallSuccess });
      if (ok) { setOpen(false); setFile(null); setSource(""); }
    } catch (reason) { setLocalError(friendlyError(errorCode(reason), locale)); }
  };
  return <div className="admin-section-stack admin-source-section"><SectionTitle icon={<Package />} title={c.pluginsTitle} body={c.pluginsBody} action={<button type="button" disabled={snapshot.configurationReadOnly} onClick={() => setOpen(true)}><Plus size={17} />{c.addPlugin}</button>} />
    {open && <div className="sheet-backdrop modal-backdrop" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget) setOpen(false); }}><article ref={dialogRef} className="setup-dialog admin-dialog" role="dialog" aria-modal="true" aria-labelledby="plugin-dialog-title"><div className="drawer-heading"><h2 id="plugin-dialog-title">{c.addPlugin}</h2><button className="icon-button neutral" type="button" aria-label={c.cancel} onClick={() => setOpen(false)}><X size={19} /></button></div><Field label={locale === "zh-CN" ? "从电脑选择" : "Choose from this computer"} {...(file?.name ? { hint: file.name } : {})}><input className="plugin-file-input" type="file" accept=".imp,application/octet-stream" onChange={(event) => { setFile(event.target.files?.[0] ?? null); if (event.target.files?.[0]) setSource(""); }} /></Field><div className="dialog-divider"><span>{locale === "zh-CN" ? "或者" : "or"}</span></div><Field label={locale === "zh-CN" ? "从 HTTPS 地址安装" : "Install from an HTTPS URL"}><input value={source} maxLength={4096} placeholder="https://…/plugin.imp" onChange={(event) => { setSource(event.target.value); if (event.target.value) setFile(null); }} /></Field>{localError && <p className="capability-feedback error" role="alert">{localError}</p>}<div className="admin-dialog-section"><h3>{locale === "zh-CN" ? "安装选项" : "Installation options"}</h3><div className="admin-form-grid"><Field label={c.scope}><ScopeSelect value={scope} onChange={setScope} locale={locale} /></Field><Field label={c.sha}><input value={sha} maxLength={64} onChange={(event) => setSha(event.target.value)} /></Field><Field label={c.signer}><input value={signer} maxLength={128} onChange={(event) => setSigner(event.target.value)} /></Field><Field label={c.fingerprint}><input value={fingerprint} maxLength={64} onChange={(event) => setFingerprint(event.target.value)} /></Field></div></div><div className="admin-form-actions"><button className="secondary" type="button" onClick={() => setOpen(false)}>{c.cancel}</button><button disabled={busy || !file && !source} type="button" onClick={() => void install()}>{c.install}</button></div></article></div>}
    {effective.length === 0 ? <EmptyState icon={<Package />} title={c.noPlugins} body={c.noPluginsBody} /> : <><div className="admin-grid">{visible.map((item) => <PluginCard key={`${item.scope}:${item.id}`} item={item} all={snapshot.plugins} busy={busy} locale={locale} readOnly={snapshot.configurationReadOnly} act={act} feedback={feedback?.target === item.id ? feedback : null} />)}</div><PageControls page={page} total={effective.length} pageSize={6} setPage={setPage} locale={locale} /></>}
  </div>;
}

function PluginCard({ item, all, busy, locale, readOnly, act, feedback }: { item: PluginAdmin; all: PluginAdmin[]; busy: boolean; locale: Locale; readOnly: boolean; act: SectionProps["act"]; feedback?: ActionFeedback | null }) {
  const c = copy(locale); const layers = all.filter((candidate) => candidate.id === item.id && candidate.scope !== "effective" && candidate.actionScope); const editableLayer = layers[0];
  return <article className="card admin-entity-card"><div className="entity-card-heading"><div className="entity-icon"><Package size={18} /></div><div><h3>{pluginDisplayName(item.id, locale)}</h3><p>{item.version && item.version !== "0.0.0" ? `${c.version} ${item.version}` : pluginDescription(item.id, locale)}</p></div><StatusBadge tone={item.enabled === false ? "neutral" : "ok"}>{item.enabled === false ? c.disabled : c.enabled}</StatusBadge></div>
    <div className="admin-form-actions entity-card-actions"><button className="secondary" disabled={busy || readOnly || !item.actionScope} type="button" onClick={() => void act({ schemaVersion: 1, action: "plugin.verify", scope: item.actionScope, target: item.id }, { success: c.pluginVerifySuccess })}>{c.verify}</button>{editableLayer?.actionScope && <button className="secondary" disabled={busy || readOnly} type="button" onClick={() => void act({ schemaVersion: 1, action: editableLayer.enabled === false ? "plugin.enable" : "plugin.disable", scope: editableLayer.actionScope, target: item.id }, { dangerous: true, ...(editableLayer.enabled === false ? {} : { confirm: locale === "zh-CN" ? "停用后，使用这项本地能力的任务将无法运行。确定停用吗？" : "Tasks using this local capability will stop working. Disable it?" }), success: c.success })}>{editableLayer.enabled === false ? c.enable : c.disable}</button>}{editableLayer?.actionScope && <button className="danger secondary" disabled={busy || readOnly} type="button" onClick={() => void act({ schemaVersion: 1, action: "plugin.remove", scope: editableLayer.actionScope, target: item.id }, { dangerous: true, confirm: c.pluginRemoveConfirm, success: c.success })}><Trash2 size={16} />{c.remove}</button>}</div>
    {feedback && <p className={`capability-feedback ${feedback.kind}`} role={feedback.kind === "error" ? "alert" : "status"}>{feedback.message}</p>}
    <details className="admin-advanced"><summary>{locale === "zh-CN" ? "查看安装信息" : "View installation information"}</summary><dl className="admin-detail-list"><Detail label="ID" value={item.id} /><Detail label={c.packageSource} value={item.source ?? c.inherited} /><Detail label={c.target} value={item.target ?? c.inherited} /><Detail label={c.verification} value={item.verification ?? c.inherited} /><Detail label={c.sha} value={item.sha256 ?? c.inherited} /><Detail label={c.signer} value={item.signingKeyId ?? c.inherited} /></dl></details>
  </article>;
}

function ConfigurationSection({ snapshot, busy, locale, act, feedback }: SectionProps) {
  const c = copy(locale); const [values, setValues] = useState(() => preferenceValues(snapshot.configuration)); const [dirty, setDirty] = useState<Set<keyof PreferenceValues>>(new Set());
  useEffect(() => { if (dirty.size === 0) setValues(preferenceValues(snapshot.configuration)); }, [dirty.size, snapshot.configuration]);
  const update = <K extends keyof PreferenceValues>(key: K, value: PreferenceValues[K]) => { setValues((current) => ({ ...current, [key]: value })); setDirty((current) => new Set(current).add(key)); };
  const save = async () => {
    for (const key of dirty) {
      const setting = preferenceSetting(key, values[key]);
      const ok = await act({ schemaVersion: 1, action: "config.set", scope: "project", target: setting.key, value: setting.value }, { dangerous: true, success: c.configSaved });
      if (!ok) return;
    }
    setDirty(new Set());
  };
  const reset = () => { setValues(preferenceDefaults); setDirty(new Set(Object.keys(preferenceDefaults) as Array<keyof PreferenceValues>)); };
  const zhLocale = locale === "zh-CN";
  const configFeedback = feedback && (feedback.target.startsWith("conversion.") || feedback.target.startsWith("cli.")) ? feedback : null;
  return <div className="admin-section-stack"><SectionTitle icon={<Settings2 />} title={c.configTitle} body={c.configBody} />
    <div className="preference-groups">
      <PreferenceGroup title={zhLocale ? "文稿与识别" : "Documents and recognition"} icon={<ScanText size={19} />}>
        <PreferenceRow label={zhLocale ? "扫描内容识别" : "OCR for scanned content"} description={zhLocale ? "自动判断是否需要识别扫描页" : "Automatically detect scanned pages"}><select value={values.ocrPolicy} onChange={(event) => update("ocrPolicy", event.target.value)}><option value="auto">{zhLocale ? "自动" : "Automatic"}</option><option value="always">{zhLocale ? "始终识别" : "Always"}</option><option value="off">{zhLocale ? "关闭" : "Off"}</option></select></PreferenceRow>
        <PreferenceRow label={zhLocale ? "识别语言" : "OCR languages"} description={zhLocale ? "多个语言用逗号分隔" : "Separate multiple languages with commas"}><input value={values.ocrLanguages} onChange={(event) => update("ocrLanguages", event.target.value)} /></PreferenceRow>
        <PreferenceRow label={zhLocale ? "最低识别置信度" : "Minimum OCR confidence"} description={zhLocale ? "低于此值的内容会被忽略" : "Content below this value is ignored"}><div className="preference-range"><input type="range" min="0" max="100" value={values.ocrConfidence} onChange={(event) => update("ocrConfidence", Number(event.target.value))} /><output>{values.ocrConfidence}%</output></div></PreferenceRow>
        <PreferenceRow label={zhLocale ? "无法解码的字符" : "Invalid text bytes"} description={zhLocale ? "遇到损坏字符时的处理方式" : "How malformed characters are handled"}><select value={values.decoding} onChange={(event) => update("decoding", event.target.value)}><option value="strict">{zhLocale ? "停止转换" : "Stop conversion"}</option><option value="replace">{zhLocale ? "替换并继续" : "Replace and continue"}</option></select></PreferenceRow>
        <PreferenceRow label={zhLocale ? "表格首行" : "Table header row"} description={zhLocale ? "决定 CSV 和 TSV 的第一行如何处理" : "How the first CSV or TSV row is handled"}><select value={values.tableHeader} onChange={(event) => update("tableHeader", event.target.value)}><option value="auto">{zhLocale ? "自动判断" : "Automatic"}</option><option value="always">{zhLocale ? "作为标题" : "Use as header"}</option><option value="never">{zhLocale ? "作为数据" : "Use as data"}</option></select></PreferenceRow>
      </PreferenceGroup>
      <PreferenceGroup title={zhLocale ? "输出结果" : "Output"} icon={<Download size={19} />}>
        <PreferenceRow label={zhLocale ? "图片与附件" : "Images and attachments"} description={zhLocale ? "选择转换结果如何保存附件" : "Choose how attachments are saved"}><select value={values.assetMode} onChange={(event) => update("assetMode", event.target.value)}><option value="extract">{zhLocale ? "保存到同名文件夹" : "Save in a companion folder"}</option><option value="embed">{zhLocale ? "直接写入 Markdown" : "Embed in Markdown"}</option><option value="omit">{zhLocale ? "不保存附件" : "Do not save attachments"}</option></select></PreferenceRow>
        <PreferenceRow label={zhLocale ? "文件重名时" : "When files already exist"} description={zhLocale ? "避免意外覆盖已有文件" : "Prevent accidental overwrites"}><select value={values.conflict} onChange={(event) => update("conflict", event.target.value)}><option value="rename">{zhLocale ? "自动重命名" : "Rename automatically"}</option><option value="error">{zhLocale ? "停止并提示" : "Stop and ask"}</option><option value="overwrite">{zhLocale ? "覆盖文件" : "Overwrite"}</option></select></PreferenceRow>
        <PreferenceRow label={zhLocale ? "保留来源信息" : "Keep source information"} description={zhLocale ? "在结果中保留页码和来源位置" : "Keep page and source locations in the result"}><label className="preference-switch"><input type="checkbox" checked={values.provenance} onChange={(event) => update("provenance", event.target.checked)} /><span /></label></PreferenceRow>
      </PreferenceGroup>
      <PreferenceGroup title={zhLocale ? "语音转写" : "Speech transcription"} icon={<Speech size={19} />} collapsed>
        <PreferenceRow label={zhLocale ? "默认语言" : "Default language"} description={zhLocale ? "留空时自动判断" : "Leave blank to detect automatically"}><input value={values.asrLanguage} onChange={(event) => update("asrLanguage", event.target.value)} /></PreferenceRow>
        <PreferenceRow label={zhLocale ? "中文输出" : "Chinese output"} description={zhLocale ? "选择中文逐字稿的字形" : "Choose Chinese transcript glyphs"}><select value={values.chineseScript} onChange={(event) => update("chineseScript", event.target.value)}><option value="preserve">{zhLocale ? "保持原样" : "Preserve"}</option><option value="simplified">{zhLocale ? "简体中文" : "Simplified"}</option><option value="traditional">{zhLocale ? "繁体中文" : "Traditional"}</option></select></PreferenceRow>
      </PreferenceGroup>
      <PreferenceGroup title={zhLocale ? "性能" : "Performance"} icon={<Activity size={19} />} collapsed>
        <PreferenceRow label={zhLocale ? "转换超时" : "Conversion timeout"} description={zhLocale ? "单个任务允许运行的最长时间" : "Maximum time for one task"}><div className="preference-with-unit"><input type="number" min="1" max="1440" value={values.timeoutMinutes} onChange={(event) => update("timeoutMinutes", Number(event.target.value))} /><span>{zhLocale ? "分钟" : "min"}</span></div></PreferenceRow>
        <PreferenceRow label={zhLocale ? "并行任务" : "Concurrent tasks"} description={zhLocale ? "同时处理的文件数量" : "Number of files processed at once"}><input type="number" min="1" max="64" value={values.jobs} onChange={(event) => update("jobs", Number(event.target.value))} /></PreferenceRow>
      </PreferenceGroup>
      <PreferenceGroup title={zhLocale ? "隐私与网络" : "Privacy and network"} icon={<ShieldCheck size={19} />} collapsed>
        <PreferenceRow label={zhLocale ? "阻止访问局域网" : "Block private networks"} description={zhLocale ? "防止远端请求访问本机或局域网地址" : "Prevent remote requests from reaching local addresses"}><label className="preference-switch"><input type="checkbox" checked={values.denyPrivateNetworks} onChange={(event) => update("denyPrivateNetworks", event.target.checked)} /><span /></label></PreferenceRow>
        <PreferenceRow label={zhLocale ? "允许访问的主机" : "Allowed hosts"} description={zhLocale ? "多个主机用逗号分隔" : "Separate multiple hosts with commas"}><input value={values.allowedHosts} onChange={(event) => update("allowedHosts", event.target.value)} /></PreferenceRow>
      </PreferenceGroup>
    </div>
    <div className="preference-savebar"><button className="secondary" disabled={busy || snapshot.configurationReadOnly} type="button" onClick={reset}>{c.restore}</button><span className={configFeedback?.kind === "error" ? "error" : dirty.size ? "changed" : ""} role={configFeedback?.kind === "error" ? "alert" : "status"}>{configFeedback?.message ?? (dirty.size ? (zhLocale ? `${dirty.size} 项更改尚未保存` : `${dirty.size} unsaved changes`) : (zhLocale ? "设置已保存" : "Preferences saved"))}</span><button disabled={busy || snapshot.configurationReadOnly || dirty.size === 0} type="button" onClick={() => void save()}><CheckCircle2 size={17} />{c.save}</button></div>
  </div>;
}

function PreferencesSkeleton({ locale }: { locale: Locale }) {
  const c = copy(locale); const zhLocale = locale === "zh-CN";
  const groups = zhLocale ? ["文稿与识别", "输出结果", "语音转写", "性能", "隐私与网络"] : ["Documents and recognition", "Output", "Speech transcription", "Performance", "Privacy and network"];
  return <div className="admin-section-stack" aria-busy="true"><SectionTitle icon={<Settings2 />} title={c.configTitle} body="" /><div className="preference-groups preference-skeleton" role="status" aria-label={c.loading}>{groups.map((title, index) => <div className="card preference-group" key={title}><div className="preference-skeleton-heading"><span className="skeleton-icon" /><strong>{title}</strong></div>{index < 2 && <div className="preference-rows"><div className="preference-row"><div><span className="skeleton-line wide" /><span className="skeleton-line" /></div><span className="skeleton-control" /></div></div>}</div>)}</div><div className="preference-savebar skeleton-savebar"><button className="secondary" type="button" disabled>{c.restore}</button><span>{c.loading}</span><button type="button" disabled>{c.save}</button></div></div>;
}

interface PreferenceValues {
  ocrPolicy: string; ocrLanguages: string; ocrConfidence: number; decoding: string; tableHeader: string;
  assetMode: string; conflict: string; provenance: boolean; asrLanguage: string; chineseScript: string;
  timeoutMinutes: number; jobs: number; denyPrivateNetworks: boolean; allowedHosts: string;
}
const preferenceDefaults: PreferenceValues = {
  ocrPolicy: "auto", ocrLanguages: "zh, en", ocrConfidence: 70, decoding: "strict", tableHeader: "auto",
  assetMode: "extract", conflict: "rename", provenance: true, asrLanguage: "", chineseScript: "preserve",
  timeoutMinutes: 10, jobs: 4, denyPrivateNetworks: true, allowedHosts: "",
};
function preferenceValues(configuration: Record<string, unknown>): PreferenceValues {
  const value = (path: string) => configAt(configuration, path);
  const languages = value("conversion.ocr.languages"); const hosts = value("conversion.network.allowed_hosts");
  return {
    ocrPolicy: stringValue(value("conversion.ocr.policy"), preferenceDefaults.ocrPolicy),
    ocrLanguages: Array.isArray(languages) ? languages.join(", ") : stringValue(languages, preferenceDefaults.ocrLanguages),
    ocrConfidence: Math.round(numberValue(value("conversion.ocr.minimum_confidence"), .7) * 100),
    decoding: stringValue(value("conversion.text.decoding_mode"), preferenceDefaults.decoding),
    tableHeader: stringValue(value("conversion.delimited_text.header"), preferenceDefaults.tableHeader),
    assetMode: stringValue(value("conversion.output.asset_mode"), preferenceDefaults.assetMode),
    conflict: stringValue(value("conversion.output.conflict"), preferenceDefaults.conflict),
    provenance: booleanValue(value("conversion.output.include_provenance"), preferenceDefaults.provenance),
    asrLanguage: stringValue(value("conversion.asr.language"), preferenceDefaults.asrLanguage),
    chineseScript: stringValue(value("conversion.asr.chinese_script"), preferenceDefaults.chineseScript),
    timeoutMinutes: Math.max(1, Math.round(numberValue(value("conversion.timeout_ms"), 600_000) / 60_000)),
    jobs: numberValue(value("cli.jobs"), preferenceDefaults.jobs),
    denyPrivateNetworks: booleanValue(value("conversion.network.deny_private_networks"), preferenceDefaults.denyPrivateNetworks),
    allowedHosts: Array.isArray(hosts) ? hosts.join(", ") : stringValue(hosts, preferenceDefaults.allowedHosts),
  };
}
function preferenceSetting(key: keyof PreferenceValues, value: PreferenceValues[keyof PreferenceValues]) {
  const mapping: Record<keyof PreferenceValues, string> = {
    ocrPolicy: "conversion.ocr.policy", ocrLanguages: "conversion.ocr.languages", ocrConfidence: "conversion.ocr.minimum_confidence",
    decoding: "conversion.text.decoding_mode", tableHeader: "conversion.delimited_text.header", assetMode: "conversion.output.asset_mode",
    conflict: "conversion.output.conflict", provenance: "conversion.output.include_provenance", asrLanguage: "conversion.asr.language",
    chineseScript: "conversion.asr.chinese_script", timeoutMinutes: "conversion.timeout_ms", jobs: "cli.jobs",
    denyPrivateNetworks: "conversion.network.deny_private_networks", allowedHosts: "conversion.network.allowed_hosts",
  };
  if (key === "ocrLanguages" || key === "allowedHosts") return { key: mapping[key], value: JSON.stringify(String(value).split(",").map((item) => item.trim()).filter(Boolean)) };
  if (key === "ocrConfidence") return { key: mapping[key], value: String(Number(value) / 100) };
  if (key === "timeoutMinutes") return { key: mapping[key], value: String(Number(value) * 60_000) };
  return { key: mapping[key], value: String(value) };
}
function configAt(configuration: Record<string, unknown>, path: string): unknown { let current: unknown = configuration; for (const part of path.split(".")) { if (!current || typeof current !== "object" || Array.isArray(current)) return undefined; current = (current as Record<string, unknown>)[part]; } return current; }
function stringValue(value: unknown, fallback: string) { return typeof value === "string" ? value : fallback; }
function numberValue(value: unknown, fallback: number) { return typeof value === "number" && Number.isFinite(value) ? value : fallback; }
function booleanValue(value: unknown, fallback: boolean) { return typeof value === "boolean" ? value : fallback; }
function PreferenceGroup({ title, icon, collapsed = false, children }: { title: string; icon: ReactNode; collapsed?: boolean; children: ReactNode }) { return <details className="card preference-group" open={!collapsed}><summary><span>{icon}</span><strong>{title}</strong></summary><div className="preference-rows">{children}</div></details>; }
function PreferenceRow({ label, description, children }: { label: string; description: string; children: ReactNode }) { return <div className="preference-row"><div><strong>{label}</strong><small>{description}</small></div><div className="preference-control">{children}</div></div>; }

function DoctorSection({ snapshot, busy, locale, act }: SectionProps) {
  const c = copy(locale); const [network, setNetwork] = useState(false); const [page, setPage] = useState(0); const checks = snapshot.operationResult?.kind === "doctor" ? snapshot.operationResult.checks : snapshot.doctor; const uniqueChecks = [...new Map(checks.map((item) => [item.id, item])).values()]; const issueGroups = groupDoctorChecks(uniqueChecks.filter((item) => !isHealthy(item) && !isSkipped(item))); const visibleIssues = issueGroups.slice(page * 5, page * 5 + 5); const passed = uniqueChecks.filter(isHealthy); const skipped = uniqueChecks.filter(isSkipped);
  return <div className="admin-section-stack"><SectionTitle icon={<Wrench />} title={c.doctorTitle} body={c.doctorBody} action={<button disabled={busy} type="button" onClick={() => void act({ schemaVersion: 1, action: "doctor.run", allowPrivateNetwork: false }, { network, success: c.doctorDone })}><Activity size={17} />{c.run}</button>} />
    <label className="admin-doctor-network"><input type="checkbox" checked={network} onChange={(event) => setNetwork(event.target.checked)} />{c.checkNetwork}</label>
    <div className={`card doctor-summary ${issueGroups.length === 0 ? "healthy" : "attention"}`}>{issueGroups.length === 0 ? <CheckCircle2 size={30} /> : <CircleAlert size={30} />}<div><h2>{issueGroups.length === 0 ? c.healthy : `${issueGroups.length} ${c.attention}`}</h2><p>{issueGroups.length === 0 ? c.healthyBody : `${passed.length} ${c.passed}${skipped.length ? ` · ${skipped.length} ${c.notRun}` : ""}`}</p></div></div>
    {issueGroups.length > 0 && <><div className="doctor-list">{visibleIssues.map((group) => <DoctorCard key={group.key} item={group.items[0]!} related={group.items} locale={locale} />)}</div><PageControls page={page} total={issueGroups.length} pageSize={5} setPage={setPage} locale={locale} /></>}
    {passed.length > 0 && <details className="card admin-advanced doctor-passed"><summary>{c.passedChecks}（{passed.length}）</summary><div className="doctor-list">{passed.map((item) => <DoctorCard key={item.id} item={item} locale={locale} />)}</div></details>}
    {skipped.length > 0 && <details className="card admin-advanced doctor-passed"><summary>{c.skippedChecks}（{skipped.length}）</summary><div className="doctor-list">{skipped.map((item) => <DoctorCard key={item.id} item={item} locale={locale} />)}</div></details>}
  </div>;
}

function DoctorCard({ item, related = [item], locale }: { item: DoctorAdmin; related?: DoctorAdmin[]; locale: Locale }) {
  const c = copy(locale); const info = doctorInfo(item, locale); const healthy = isHealthy(item); const skipped = isSkipped(item);
  return <article className="card doctor-card"><div className="doctor-card-heading">{healthy ? <CheckCircle2 size={20} /> : <CircleAlert size={20} />}<div><h3>{info.title}</h3><StatusBadge tone={healthy ? "ok" : skipped ? "neutral" : "warning"}>{friendlyStatus(item.status, locale)}</StatusBadge></div>{!healthy && !skipped && <RouteLink href={info.href} className="secondary doctor-action">{info.actionLabel}</RouteLink>}</div>{related.length > 1 && <div className="chip-row" aria-label={locale === "zh-CN" ? "受影响能力" : "Affected capabilities"}>{related.map((check) => <span className="status-pill" key={check.id}>{doctorAffectedLabel(check, locale)}</span>)}</div>}{!healthy && !skipped && <div className="doctor-guidance"><div><strong>{c.impact}</strong><p>{info.impact}</p></div><div><strong>{c.nextStep}</strong><p>{info.action}</p></div></div>}<details className="admin-advanced"><summary>{c.technical}</summary>{related.map((check) => <div key={check.id}><p><code>{check.id}</code></p><p className="breakable">{check.detail}</p></div>)}</details></article>;
}

function SectionTitle({ icon, title, body, action }: { icon: ReactNode; title: string; body: string; action?: ReactNode }) { return <header className="admin-section-title"><div className="admin-section-icon">{icon}</div><div><h2>{title}</h2>{body && <p>{body}</p>}</div>{action && <div className="admin-section-action">{action}</div>}</header>; }
function Field({ label, hint, children }: { label: string; hint?: string; children: ReactNode }) { return <label className="admin-field"><span>{label}</span>{children}{hint && <small>{hint}</small>}</label>; }
function CheckField({ label, checked, setChecked }: { label: string; checked: boolean; setChecked: (value: boolean) => void }) { return <label className="check admin-check"><input type="checkbox" checked={checked} onChange={(event) => setChecked(event.target.checked)} /><span>{label}</span></label>; }
function ScopeSelect({ value, onChange, locale }: { value: "global" | "project"; onChange: (value: "global" | "project") => void; locale: Locale }) { const c = copy(locale); return <select value={value} onChange={(event) => onChange(event.target.value === "global" ? "global" : "project")}><option value="project">{c.project}</option><option value="global">{c.global}</option></select>; }
function Feedback({ kind, children }: { kind: "error" | "success" | "readonly"; children: ReactNode }) { return <div className={`admin-feedback ${kind}`} role={kind === "error" ? "alert" : "status"}>{children}</div>; }
function StatusBadge({ tone, children }: { tone: "ok" | "warning" | "neutral"; children: ReactNode }) { return <span className={`admin-status ${tone}`}>{children}</span>; }
function Detail({ label, value }: { label: string; value: string }) { return <div><dt>{label}</dt><dd className="breakable">{value}</dd></div>; }
function EmptyState({ icon, title, body }: { icon: ReactNode; title: string; body: string }) { return <article className="card admin-empty"><div className="entity-icon">{icon}</div><h2>{title}</h2><p>{body}</p></article>; }
function PageControls({ page, total, pageSize, setPage, locale }: { page: number; total: number; pageSize: number; setPage: (page: number) => void; locale: Locale }) { const pages = Math.max(1, Math.ceil(total / pageSize)); if (pages <= 1) return null; return <nav className="admin-pagination" aria-label={locale === "zh-CN" ? "翻页" : "Pagination"}><span>{locale === "zh-CN" ? `第 ${page + 1} / ${pages} 页` : `Page ${page + 1} of ${pages}`}</span><div><button className="secondary" type="button" disabled={page === 0} onClick={() => setPage(Math.max(0, page - 1))}>{locale === "zh-CN" ? "上一页" : "Previous"}</button><button className="secondary" type="button" disabled={page + 1 >= pages} onClick={() => setPage(Math.min(pages - 1, page + 1))}>{locale === "zh-CN" ? "下一页" : "Next"}</button></div></nav>; }

function csv(value: string) { return value.split(",").map((item) => item.trim()).filter(Boolean); }
function errorCode(reason: unknown) { return reason instanceof Error && "code" in reason ? String((reason as { code: unknown }).code) : "requestFailed"; }
function friendlyError(code: string, locale: Locale) { const chinese = locale === "zh-CN"; const known: Record<string, [string, string]> = { unreachable: ["无法连接本地服务，请确认 into-md 仍在运行。", "Cannot reach the local service. Make sure into-md is still running."], requestFailed: ["操作没有完成，请重试。", "The operation did not complete. Try again."], authorizationRequired: ["本次操作需要重新确认，请再试一次。", "This action needs fresh confirmation. Try again."], networkAuthorizationRequired: ["本次测试需要明确授权联网，请重新测试并确认联网。", "This test needs explicit network permission. Run it again and approve network access."], privateNetworkDenied: ["连接被安全策略阻止：这是局域网地址。编辑该 AI 服务并允许访问局域网，然后重试。", "The security policy blocked this private-network address. Edit the AI service, allow local-network access, then retry."], providerSecretMissing: ["未找到这个 AI 服务所需的密钥环境变量。请设置密钥后重新启动 into-md，再测试连接。", "The environment variable for this AI service key is missing. Set it, restart into-md, then test again."], invalidAction: ["当前输入无法执行，请检查后重试。", "The current input cannot be used. Check it and try again."], configurationReadOnly: ["当前为只读模式，不能修改设置。", "Settings cannot be changed in view-only mode."] }; return (known[code] ?? ["操作未完成，请检查当前设置后重试。", "The operation did not complete. Check the current settings and try again."])[chinese ? 0 : 1]; }
function friendlyFamily(value: string, locale: Locale) { const zhNames: Record<string, string> = { document: "文档", text: "文本", image: "图片", audio: "音频", video: "视频", archive: "压缩包", data: "数据", presentation: "演示文稿", spreadsheet: "表格" }; return locale === "zh-CN" ? zhNames[value.toLowerCase()] ?? value : value; }
function friendlyStatus(value: string, locale: Locale) { if (value.toLowerCase() === "skipped") return locale === "zh-CN" ? "未检查" : "Not checked"; const healthy = ["ok", "pass", "passed", "ready", "healthy", "available"].includes(value.toLowerCase()); return healthy ? copy(locale).ready : locale === "zh-CN" ? "需要处理" : "Needs attention"; }
function friendlyCheckStage(value: string, locale: Locale) { const chinese = locale === "zh-CN"; const names: Record<string, [string, string]> = { queued: ["等待验证", "Waiting"], package: ["检查插件包", "Checking package"], runtime: ["检查运行环境", "Checking runtime"], models: ["测试内置模型", "Testing bundled models"], cancelling: ["正在取消", "Cancelling"], completed: ["验证完成", "Completed"] }; return names[value]?.[chinese ? 0 : 1] ?? value; }
function isHealthy(item: DoctorAdmin) { return ["ok", "pass", "passed", "ready", "healthy", "available"].includes(item.status.toLowerCase()); }
function isSkipped(item: DoctorAdmin) { return item.status.toLowerCase() === "skipped"; }
function doctorRemediationKey(item: DoctorAdmin) {
  const id = item.id.toLowerCase();
  if (id === "runtime.asr" || id === "runtime.diarization" || id.includes("official.media.whisper")) return "plugin:media";
  if (id === "runtime.ocr" || id.includes("official.ocr.ppocrv6")) return "plugin:ocr";
  if (id === "runtime.legacy-office" || id.includes("official.legacy-office")) return "plugin:legacy-office";
  if (id.startsWith("providerenvironment:") || id.startsWith("provider:")) return `provider:${id.slice(id.indexOf(":") + 1).split(/[/.]/, 1)[0]}`;
  if (id.includes("provider") || id.includes("api")) return "provider:connection";
  if (id.includes("plugin")) return `plugin:${id.replace(/^.*plugin[:.]/, "").split(/[/.]/, 1)[0]}`;
  if (id.includes("config")) return "preferences";
  if (id.includes("network")) return "network";
  return id;
}
function groupDoctorChecks(items: DoctorAdmin[]) {
  const groups = new Map<string, DoctorAdmin[]>();
  for (const item of items) groups.set(doctorRemediationKey(item), [...(groups.get(doctorRemediationKey(item)) ?? []), item]);
  return [...groups].map(([key, grouped]) => ({ key, items: grouped }));
}
function doctorAffectedLabel(item: DoctorAdmin, locale: Locale) {
  const id = item.id.toLowerCase(); const chinese = locale === "zh-CN";
  if (id.includes("diarization")) return chinese ? "说话人识别" : "Speaker identification";
  if (id.includes("asr") || id.includes("transcription")) return chinese ? "语音转写" : "Speech transcription";
  if (id.includes("ocr")) return chinese ? "图片 OCR" : "Image OCR";
  if (id.includes("legacy")) return chinese ? "旧版 Office" : "Legacy Office";
  return item.id.replaceAll(/[._-]+/g, " ");
}
function friendlyCapabilityStatus(value: CapabilityAdmin["status"], locale: Locale) { const chinese = locale === "zh-CN"; const names: Record<CapabilityAdmin["status"], [string, string]> = { "not-installed": ["未安装", "Not installed"], downloading: ["下载中", "Downloading"], verifying: ["验证中", "Verifying"], ready: ["可用", "Ready"], "update-available": ["可更新", "Update available"], corrupt: ["需要修复", "Needs repair"], incompatible: ["不兼容", "Incompatible"], blocked: ["已阻止", "Blocked"] }; return names[value][chinese ? 0 : 1]; }
function friendlyCapabilitySource(value: string, locale: Locale) { const c = copy(locale); if (value === "off") return c.off; const [, identity = value] = value.split(":", 2); const name = identity.split("/", 1)[0] ?? identity; return value.startsWith("plugin:") ? `${c.localPlugin} · ${pluginDisplayName(name, locale)}` : `${c.remoteProvider} · ${name}`; }
function friendlyProviderCapability(value: string, locale: Locale) { const chinese = locale === "zh-CN"; const names: Record<string, [string, string]> = { "vision-ocr": ["图片 OCR", "Image OCR"], ocr: ["图片 OCR", "Image OCR"], "image-description": ["图片理解", "Image understanding"], "audio-transcription": ["语音转写", "Speech transcription"], transcription: ["语音转写", "Speech transcription"], diarization: ["说话人识别", "Speaker identification"], chat: ["文本生成", "Text generation"], vision: ["视觉理解", "Vision"] }; return names[value]?.[chinese ? 0 : 1] ?? value; }
function pluginDisplayName(id: string, locale: Locale) { const chinese = locale === "zh-CN"; const names: Record<string, [string, string]> = { "official.legacy-office.libreoffice": ["旧版 Office 转换", "Legacy Office conversion"], "official.ocr.ppocrv6": ["本地 OCR（PP-OCR）", "Local OCR (PP-OCR)"], "official.media.whisper": ["本地语音（Whisper）", "Local speech (Whisper)"] }; return names[id]?.[chinese ? 0 : 1] ?? id; }
function pluginDescription(id: string, locale: Locale) { const chinese = locale === "zh-CN"; if (id.includes("legacy-office")) return chinese ? "转换 .doc、.xls 和 .ppt 文件" : "Converts .doc, .xls, and .ppt files"; if (id.includes("ocr")) return chinese ? "识别扫描 PDF 和图片中的文字" : "Recognizes text in scans and images"; if (id.includes("media") || id.includes("whisper")) return chinese ? "语音转写与说话人识别" : "Speech transcription and speaker identification"; return chinese ? "本地扩展能力" : "Local extension capability"; }
function doctorInfo(item: DoctorAdmin, locale: Locale) {
  const chinese = locale === "zh-CN"; const id = item.id.toLowerCase(); const capabilityHref = "/admin/capabilities"; const preferenceHref = "/admin/configuration";
  const capabilityAction = chinese ? "前往能力与来源" : "Open capabilities"; const preferenceAction = chinese ? "前往偏好设置" : "Open preferences";
  if (id === "runtime.pdfium") return { title: chinese ? "PDF 解析组件" : "PDF parser", impact: chinese ? "PDF 文件可能无法转换。" : "PDF files may not convert.", action: chinese ? "修复 Core 中的 PDFium 组件，然后重新检查。" : "Repair the PDFium component, then run diagnostics again.", href: preferenceHref, actionLabel: preferenceAction };
  if (["runtime.ocr", "runtime.legacy-office", "runtime.asr", "runtime.diarization"].includes(id)) { const title = id === "runtime.ocr" ? (chinese ? "OCR 能力" : "OCR capability") : id === "runtime.legacy-office" ? (chinese ? "旧版 Office 能力" : "Legacy Office capability") : id === "runtime.asr" ? (chinese ? "语音转写能力" : "Transcription capability") : (chinese ? "说话人识别能力" : "Speaker identification capability"); return { title, impact: chinese ? "对应的本地转换功能无法使用。" : "The related local conversion feature is unavailable.", action: chinese ? "选择可用来源，或安装、修复对应的本地扩展。" : "Choose an available source, or install or repair its local extension.", href: capabilityHref, actionLabel: capabilityAction }; }
  if (id.startsWith("providerenvironment:")) { const name = item.id.slice(item.id.indexOf(":") + 1); return { title: `${chinese ? "AI 服务" : "AI service"} · ${name}`, impact: chinese ? "这个 AI 服务需要的密钥环境变量没有就绪。" : "The environment variable required by this AI service is not available.", action: chinese ? "编辑这个 AI 服务，检查密钥环境变量名称和当前进程环境。" : "Edit this AI service and check its key environment variable.", href: "/admin/providers", actionLabel: chinese ? "检查 AI 服务" : "Check AI service" }; }
  if (id.includes("provider") || id.includes("api")) return { title: chinese ? "AI 服务连接" : "AI service connection", impact: chinese ? "远端能力无法运行。" : "Remote capabilities cannot run.", action: chinese ? "检查服务地址、模型映射和密钥环境变量。" : "Check the service address, model mappings, and key environment variable.", href: "/admin/providers", actionLabel: chinese ? "检查 AI 服务" : "Check AI service" };
  if (id.includes("plugin")) return { title: chinese ? "本地扩展" : "Local extension", impact: chinese ? "对应的本地能力可能不可用。" : "The corresponding local capability may be unavailable.", action: chinese ? "验证、启用或重新安装对应扩展。" : "Verify, enable, or reinstall the extension.", href: "/admin/plugins", actionLabel: chinese ? "修复本地扩展" : "Repair local extension" };
  if (id.includes("config")) return { title: chinese ? "偏好设置" : "Preferences", impact: chinese ? "部分设置可能没有生效。" : "Some settings may not be applied.", action: chinese ? "检查偏好设置并保存。" : "Review and save preferences.", href: preferenceHref, actionLabel: preferenceAction };
  if (id.includes("network")) return { title: chinese ? "网络访问检查" : "Network check", impact: chinese ? "本次没有检查联网能力。" : "Network access was not checked this time.", action: chinese ? "需要使用 AI 服务或下载扩展时，启用联网检查后重试。" : "Enable the network check when using AI services or downloading extensions.", href: preferenceHref, actionLabel: preferenceAction };
  return { title: item.id.replaceAll(/[._-]+/g, " "), impact: chinese ? "相关功能可能无法正常工作。" : "The related feature may not work correctly.", action: chinese ? "查看原因，完成处理后重新检查。" : "Review the cause, address it, and run diagnostics again.", href: preferenceHref, actionLabel: preferenceAction };
}
