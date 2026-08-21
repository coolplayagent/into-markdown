import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import {
  Activity, CheckCircle2, CircleAlert, Cloud, Download, FileSearch, HardDrive,
  Package, Plus, Search, Settings2, ShieldCheck, Sparkles, Wrench,
} from "lucide-react";
import type {
  AdminAction, AdminOperationResult, AdminSnapshot, ApiClient, DoctorAdmin,
  FormatAdmin, ModelAdmin, PluginAdmin, ProviderAdmin,
} from "./api";
import { useI18n, type Locale } from "./i18n";
import { RouteLink } from "./router";

export type AdminSection = "formats" | "models" | "providers" | "plugins" | "configuration" | "doctor";
export const adminSections: AdminSection[] = ["formats", "models", "providers", "plugins", "configuration", "doctor"];

interface ActionOptions { dangerous?: boolean; network?: boolean; confirm?: string; success: string }
interface SectionProps {
  snapshot: AdminSnapshot;
  busy: boolean;
  locale: Locale;
  act: (action: AdminAction, options: ActionOptions) => Promise<void>;
}

const adminConfigKeys = [
  "cli.language", "cli.jobs", "cli.color", "cli.progress", "cli.log_format", "conversion.timeout_ms",
  "conversion.text.decoding_mode", "conversion.delimited_text.header", "conversion.delimited_text.ragged_rows",
  "conversion.ocr.policy", "conversion.ocr.model_bundle", "conversion.ocr.languages", "conversion.ocr.minimum_confidence",
  "conversion.asr.model_bundle", "conversion.asr.language", "conversion.asr.chinese_script", "conversion.asr.max_threads",
  "conversion.asr.max_duration_ms", "conversion.asr.max_segments", "conversion.asr.max_native_memory_bytes",
  "conversion.ai.vision_ocr", "conversion.ai.image_description", "conversion.ai.layout_repair", "conversion.ai.table_repair",
  "conversion.ai.formula_repair", "conversion.ai.audio_transcription", "conversion.ai.markdown_postprocess",
  "conversion.ai.provider", "conversion.ai.model", "conversion.network.max_redirects", "conversion.network.allowed_hosts",
  "conversion.network.deny_private_networks", "conversion.limits.max_input_bytes", "conversion.limits.max_decompressed_bytes",
  "conversion.limits.max_archive_entries", "conversion.limits.max_archive_depth", "conversion.limits.max_archive_entry_bytes",
  "conversion.limits.max_archive_compression_ratio", "conversion.limits.max_nesting_depth", "conversion.limits.max_pages",
  "conversion.limits.max_asset_bytes", "conversion.limits.max_total_asset_bytes", "conversion.limits.max_memory_bytes",
  "conversion.limits.max_temporary_bytes", "conversion.limits.max_table_rows", "conversion.limits.max_table_columns",
  "conversion.limits.max_table_cells", "conversion.limits.max_field_bytes", "conversion.output.emit",
  "conversion.output.asset_mode", "conversion.output.conflict", "conversion.output.asset_directory_suffix",
  "conversion.output.include_provenance",
] as const;

const zh = {
  heading: "系统管理", intro: "查看转换能力、连接 AI 服务，并处理本机运行问题。",
  tabs: { formats: "格式支持", models: "本地模型", providers: "AI 服务", plugins: "扩展插件", configuration: "设置", doctor: "运行诊断" },
  loading: "正在读取本机状态…", retry: "重新加载", viewOnly: "当前只能查看", viewOnlyBody: "本次启动未开放修改权限。你仍可以查看状态和运行只读检查。",
  success: "操作已完成", advanced: "高级选项", technical: "技术详情", cancel: "取消", add: "添加", save: "保存", remove: "删除", verify: "验证", install: "安装", test: "测试连接", setDefault: "设为默认", enable: "启用", disable: "停用", show: "查看", path: "查看位置", restore: "恢复默认", create: "创建", copy: "复制自",
  formatsTitle: "识别文件格式", formatsBody: "输入一个本地文件路径，确认它能否被转换。", localPath: "文件路径", localPathHint: "例如 /Users/me/Documents/report.pdf", detect: "开始识别", charset: "文本编码", formatHint: "指定格式", extension: "文件扩展名", mime: "MIME 类型", hosts: "允许访问的主机", privateNetwork: "允许访问局域网地址", formatLibrary: "支持的格式", formatSearch: "搜索格式或扩展名", allFormats: "全部格式", needsRuntime: "需要额外组件", ready: "可用", unavailable: "不可用", source: "来源", extensions: "扩展名", runtime: "所需组件",
  modelsTitle: "本地模型", modelsBody: "OCR、语音识别等能力依赖这些模型。模型只保存在本机。", installed: "已安装", notInstalled: "未安装", defaultModel: "默认模型", downloadOptions: "下载选项", insecure: "允许使用不安全的 HTTP 下载", modelRemoveConfirm: "确定删除这个本地模型吗？需要时必须重新下载。",
  providersTitle: "AI 服务", providersBody: "连接兼容 OpenAI 接口的服务，用于视觉识别、内容修复等可选能力。纯本地转换不需要配置。", noProviders: "尚未连接 AI 服务", noProvidersBody: "文档、OCR 和语音能力仍可在本机运行。只有需要云端 AI 增强时才添加服务。", addProvider: "连接 AI 服务", serviceName: "服务名称", baseUrl: "API 地址", model: "默认模型", apiKeyEnv: "密钥环境变量", apiKeyHint: "例如 DASHSCOPE_API_KEY。这里只保存变量名，不保存密钥。", capabilities: "支持的能力", timeout: "超时时间（毫秒）", scope: "保存位置", project: "当前项目", global: "所有项目", environmentReady: "密钥已就绪", environmentMissing: "未找到密钥", inherited: "沿用上层设置", effective: "当前生效", overridden: "已被覆盖", providerRemoveConfirm: "确定删除这个 AI 服务配置吗？", providerDefaultSuccess: "已设为默认 AI 服务", providerAddedSuccess: "AI 服务已添加", providerTestSuccess: "连接测试已完成",
  pluginsTitle: "扩展插件", pluginsBody: "插件用于增加额外格式或处理能力。内置的 OCR、语音等官方能力无需在这里安装。", noPlugins: "没有额外插件", noPluginsBody: "当前只使用内置能力，这是正常状态。需要第三方扩展时再安装插件。", addPlugin: "安装扩展插件", packageSource: "插件包路径或 HTTPS 地址", sha: "文件校验值（SHA-256）", signer: "签名方 ID", fingerprint: "签名指纹", pluginRemoveConfirm: "确定删除这个插件吗？", pluginInstallSuccess: "插件已安装", pluginVerifySuccess: "插件验证已完成", enabled: "已启用", disabled: "已停用", version: "版本", target: "适用平台", protocol: "协议", verification: "验证状态",
  configTitle: "设置", configBody: "调整转换行为。常用设置可以直接修改，内部名称和原始配置收在详情中。", chooseSetting: "选择设置", value: "设置值", readCurrent: "读取当前值", promptName: "提示词名称", addPrompt: "选择提示词设置", profiles: "设置方案", newProfile: "新方案名称", copyFrom: "复制已有方案", noProfiles: "还没有自定义设置方案。", configTools: "检查与迁移", configToolsBody: "用于定位配置文件、验证已有配置或初始化新配置。", validationPath: "要验证的配置文件", resolved: "显示合并后的最终值", force: "覆盖已有配置文件", paths: "配置文件位置", validate: "验证配置", initialize: "初始化配置", rawConfig: "查看完整配置（已隐藏敏感值）", configSaved: "设置已保存", configRestored: "已恢复默认设置", profileCreated: "设置方案已创建", profileRemoveConfirm: "确定删除这个设置方案吗？",
  doctorTitle: "运行诊断", doctorBody: "检查本机运行环境，并给出可以直接执行的处理建议。", run: "重新检查", checkNetwork: "同时检查联网能力", healthy: "没有发现问题", healthyBody: "当前检查项目均正常。", attention: "项需要处理", passed: "项正常", notRun: "项未检查", passedChecks: "查看正常项目", skippedChecks: "查看未检查项目", impact: "影响", nextStep: "处理建议", doctorDone: "诊断已完成",
  detectionResult: "识别结果", confidence: "匹配度", providerResult: "连接结果", availableModels: "可用模型", result: "操作结果", rawResult: "查看原始结果",
};
const en: typeof zh = {
  heading: "System management", intro: "Check conversion capabilities, connect AI services, and resolve local runtime issues.",
  tabs: { formats: "Format support", models: "Local models", providers: "AI services", plugins: "Extensions", configuration: "Settings", doctor: "Diagnostics" },
  loading: "Reading local status…", retry: "Reload", viewOnly: "View-only mode", viewOnlyBody: "This launch does not allow configuration changes. You can still inspect status and run read-only checks.",
  success: "Done", advanced: "Advanced options", technical: "Technical details", cancel: "Cancel", add: "Add", save: "Save", remove: "Remove", verify: "Verify", install: "Install", test: "Test connection", setDefault: "Set as default", enable: "Enable", disable: "Disable", show: "Show", path: "Show location", restore: "Restore default", create: "Create", copy: "Copy from",
  formatsTitle: "Identify a file format", formatsBody: "Enter a local file path to confirm whether it can be converted.", localPath: "File path", localPathHint: "For example, /Users/me/Documents/report.pdf", detect: "Identify format", charset: "Text encoding", formatHint: "Specify format", extension: "File extension", mime: "MIME type", hosts: "Allowed hosts", privateNetwork: "Allow local-network addresses", formatLibrary: "Supported formats", formatSearch: "Search formats or extensions", allFormats: "All formats", needsRuntime: "Additional component required", ready: "Available", unavailable: "Unavailable", source: "Source", extensions: "Extensions", runtime: "Required component",
  modelsTitle: "Local models", modelsBody: "OCR, transcription, and related capabilities use these models. Models stay on this computer.", installed: "Installed", notInstalled: "Not installed", defaultModel: "Default model", downloadOptions: "Download options", insecure: "Allow insecure HTTP downloads", modelRemoveConfirm: "Remove this local model? It must be downloaded again before use.",
  providersTitle: "AI services", providersBody: "Connect OpenAI-compatible services for optional vision, repair, and enhancement tasks. Local conversion works without one.", noProviders: "No AI service connected", noProvidersBody: "Documents, OCR, and transcription can still run locally. Add a service only when cloud AI enhancement is needed.", addProvider: "Connect AI service", serviceName: "Service name", baseUrl: "API address", model: "Default model", apiKeyEnv: "API key environment variable", apiKeyHint: "For example, DASHSCOPE_API_KEY. Only the variable name is saved; the key is not stored here.", capabilities: "Capabilities", timeout: "Timeout (milliseconds)", scope: "Save for", project: "This project", global: "All projects", environmentReady: "API key ready", environmentMissing: "API key not found", inherited: "Inherited", effective: "Active", overridden: "Overridden", providerRemoveConfirm: "Remove this AI service configuration?", providerDefaultSuccess: "Default AI service updated", providerAddedSuccess: "AI service added", providerTestSuccess: "Connection test completed",
  pluginsTitle: "Extensions", pluginsBody: "Plugins add extra formats or processing capabilities. Built-in OCR, transcription, and other official capabilities do not need installation here.", noPlugins: "No extra plugins", noPluginsBody: "Using built-in capabilities only is a normal state. Install a plugin when a third-party extension is needed.", addPlugin: "Install extension", packageSource: "Plugin package path or HTTPS URL", sha: "File checksum (SHA-256)", signer: "Signer ID", fingerprint: "Signing fingerprint", pluginRemoveConfirm: "Remove this plugin?", pluginInstallSuccess: "Plugin installed", pluginVerifySuccess: "Plugin verification completed", enabled: "Enabled", disabled: "Disabled", version: "Version", target: "Platform", protocol: "Protocol", verification: "Verification",
  configTitle: "Settings", configBody: "Adjust conversion behavior. Common values are editable here; internal names and raw configuration stay in details.", chooseSetting: "Choose a setting", value: "Value", readCurrent: "Read current value", promptName: "Prompt name", addPrompt: "Select prompt setting", profiles: "Setting profiles", newProfile: "New profile name", copyFrom: "Copy an existing profile", noProfiles: "No custom profiles yet.", configTools: "Inspect and migrate", configToolsBody: "Locate configuration files, validate existing configuration, or initialize a new file.", validationPath: "Configuration file to validate", resolved: "Show fully resolved values", force: "Overwrite an existing configuration file", paths: "Configuration locations", validate: "Validate configuration", initialize: "Initialize configuration", rawConfig: "View full configuration (secrets hidden)", configSaved: "Setting saved", configRestored: "Default restored", profileCreated: "Profile created", profileRemoveConfirm: "Remove this setting profile?",
  doctorTitle: "Diagnostics", doctorBody: "Check the local runtime and get concrete steps for anything that needs attention.", run: "Run again", checkNetwork: "Also check network access", healthy: "No issues found", healthyBody: "All current checks passed.", attention: "need attention", passed: "passed", notRun: "not checked", passedChecks: "View passing checks", skippedChecks: "View checks not run", impact: "Impact", nextStep: "What to do", doctorDone: "Diagnostics completed",
  detectionResult: "Detection result", confidence: "Confidence", providerResult: "Connection result", availableModels: "Available models", result: "Operation result", rawResult: "View raw result",
};
function copy(locale: Locale) { return locale === "zh-CN" ? zh : en; }

export function AdminPage({ api, section }: { api: ApiClient; section: AdminSection }) {
  const { locale } = useI18n();
  const c = copy(locale);
  const [snapshot, setSnapshot] = useState<AdminSnapshot | null>(null);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [attempt, setAttempt] = useState(0);
  const [busy, setBusy] = useState(false);
  const actionInFlight = useRef(false);

  useEffect(() => { setNotice(""); }, [section]);
  useEffect(() => {
    const controller = new AbortController();
    setError("");
    void api.admin(controller.signal).then(setSnapshot, (reason: unknown) => {
      if (!controller.signal.aborted) setError(errorCode(reason));
    });
    return () => controller.abort();
  }, [api, attempt]);

  const act = async (action: AdminAction, options: ActionOptions) => {
    if (actionInFlight.current || options.confirm && !window.confirm(options.confirm)) return;
    actionInFlight.current = true;
    setBusy(true); setError(""); setNotice("");
    const requested = { ...action, schemaVersion: 1 as const, authorizeDangerous: options.dangerous === true, authorizeNetwork: options.network === true };
    try {
      const authorizationGrant = options.dangerous || options.network ? await api.adminGrant(requested) : undefined;
      const outcome = await api.adminAction({ ...requested, ...(authorizationGrant ? { authorizationGrant } : {}) });
      const operationResult = outcome.operationResult;
      if (operationResult) setSnapshot((current) => current ? { ...current, operationResult } : current);
      else setSnapshot(await api.admin());
      setNotice(options.success);
    } catch (reason) { setError(errorCode(reason)); }
    finally { actionInFlight.current = false; setBusy(false); }
  };

  return <section className="admin-page">
    <div className="page-heading"><p className="eyebrow">into-md</p><h1>{c.heading}</h1><p>{c.intro}</p></div>
    <nav className="admin-tabs" aria-label={c.heading}>
      {adminSections.map((item) => <RouteLink key={item} href={`/admin/${item}`} className={section === item ? "active" : ""}>{c.tabs[item]}</RouteLink>)}
    </nav>
    {error && <Feedback kind="error"><strong>{friendlyError(error, locale)}</strong><button type="button" className="secondary" onClick={() => setAttempt((value) => value + 1)}>{c.retry}</button></Feedback>}
    {notice && <Feedback kind="success"><CheckCircle2 size={18} /><span>{notice}</span></Feedback>}
    {!snapshot ? <div className="admin-loading" role="status"><Activity className="spinner" size={18} /><span>{c.loading}</span></div> : <>
      {snapshot.configurationReadOnly && (section === "providers" || section === "plugins" || section === "configuration") && <Feedback kind="readonly"><ShieldCheck size={20} /><div><strong>{c.viewOnly}</strong><p>{c.viewOnlyBody}</p></div></Feedback>}
      {section === "formats" && <FormatsSection snapshot={snapshot} busy={busy} locale={locale} act={act} />}
      {section === "models" && <ModelsSection snapshot={snapshot} busy={busy} locale={locale} act={act} />}
      {section === "providers" && <ProvidersSection snapshot={snapshot} busy={busy} locale={locale} act={act} />}
      {section === "plugins" && <PluginsSection snapshot={snapshot} busy={busy} locale={locale} act={act} />}
      {section === "configuration" && <ConfigurationSection snapshot={snapshot} busy={busy} locale={locale} act={act} />}
      {section === "doctor" && <DoctorSection snapshot={snapshot} busy={busy} locale={locale} act={act} />}
    </>}
  </section>;
}

function FormatsSection({ snapshot, busy, locale, act }: SectionProps) {
  const c = copy(locale);
  const [source, setSource] = useState(""); const [charset, setCharset] = useState("");
  const [hint, setHint] = useState(""); const [extension, setExtension] = useState(""); const [mime, setMime] = useState("");
  const [hosts, setHosts] = useState(""); const [privateNetwork, setPrivateNetwork] = useState(false); const [search, setSearch] = useState("");
  const formats = useMemo(() => snapshot.formats.filter((item) => `${item.format} ${item.family} ${item.extensions.join(" ")}`.toLowerCase().includes(search.toLowerCase().trim())), [snapshot.formats, search]);
  return <div className="admin-section-stack">
    <SectionTitle icon={<FileSearch />} title={c.formatsTitle} body={c.formatsBody} />
    <article className="card admin-tool-card">
      <Field label={c.localPath} hint={c.localPathHint}><input value={source} maxLength={4096} onChange={(event) => setSource(event.target.value)} /></Field>
      <details className="admin-advanced"><summary>{c.advanced}</summary><div className="admin-form-grid">
        <Field label={c.formatHint}><input value={hint} maxLength={64} onChange={(event) => setHint(event.target.value)} /></Field>
        <Field label={c.extension}><input value={extension} maxLength={32} onChange={(event) => setExtension(event.target.value)} /></Field>
        <Field label={c.mime}><input value={mime} maxLength={128} onChange={(event) => setMime(event.target.value)} /></Field>
        <Field label={c.charset}><input value={charset} maxLength={128} onChange={(event) => setCharset(event.target.value)} /></Field>
        <Field label={c.hosts}><input value={hosts} maxLength={4096} onChange={(event) => setHosts(event.target.value)} /></Field>
        <CheckField label={c.privateNetwork} checked={privateNetwork} setChecked={setPrivateNetwork} />
      </div></details>
      <div className="admin-form-actions"><button disabled={busy || !source} type="button" onClick={() => void act({ schemaVersion: 1, action: "format.detect", source, ...(charset ? { charset } : {}), ...(hint ? { formatHint: hint } : {}), ...(extension ? { extension } : {}), ...(mime ? { mimeType: mime } : {}), allowHosts: csv(hosts), allowPrivateNetwork: privateNetwork }, { network: /^https?:\/\//i.test(source), dangerous: privateNetwork, success: c.success })}><Search size={17} />{c.detect}</button></div>
      <OperationResult result={snapshot.operationResult} locale={locale} only="detection" />
    </article>
    <div className="admin-list-heading"><div><h2>{c.formatLibrary}</h2><p>{snapshot.formats.length} {c.allFormats.toLowerCase()}</p></div><label className="admin-search"><Search size={16} /><span className="sr-only">{c.formatSearch}</span><input placeholder={c.formatSearch} value={search} onChange={(event) => setSearch(event.target.value)} /></label></div>
    <div className="admin-grid">{formats.map((item) => <FormatCard key={item.format} item={item} locale={locale} />)}</div>
  </div>;
}

function FormatCard({ item, locale }: { item: FormatAdmin; locale: Locale }) {
  const c = copy(locale); const ready = item.status === "supported" || item.status === "available" || !item.runtimeComponent;
  return <article className="card admin-entity-card"><div className="entity-card-heading"><div className="entity-icon"><FileSearch size={18} /></div><div><h3>{item.format}</h3><p>{friendlyFamily(item.family, locale)}</p></div><StatusBadge tone={ready ? "ok" : "warning"}>{ready ? c.ready : c.needsRuntime}</StatusBadge></div>
    <div className="chip-row">{item.extensions.map((value) => <span className="status-pill" key={value}>{value}</span>)}</div>
    {item.runtimeComponent && <details className="admin-advanced"><summary>{c.technical}</summary><dl className="admin-detail-list"><Detail label={c.runtime} value={item.runtimeComponent} /><Detail label={c.source} value={item.source} />{item.installHint && <Detail label={c.install} value={item.installHint} />}</dl></details>}
  </article>;
}

function ModelsSection({ snapshot, busy, locale, act }: SectionProps) {
  const c = copy(locale); const [privateNetwork, setPrivateNetwork] = useState(false); const [insecure, setInsecure] = useState(false);
  return <div className="admin-section-stack"><SectionTitle icon={<HardDrive />} title={c.modelsTitle} body={c.modelsBody} />
    <details className="card admin-advanced admin-page-options"><summary>{c.downloadOptions}</summary><div className="admin-form-row"><CheckField label={c.privateNetwork} checked={privateNetwork} setChecked={setPrivateNetwork} /><CheckField label={c.insecure} checked={insecure} setChecked={setInsecure} /></div></details>
    <div className="admin-grid">{snapshot.models.entries.map((item) => <ModelCard key={item.bundle.id} item={item} defaultId={snapshot.models.defaultBundle} busy={busy} locale={locale} privateNetwork={privateNetwork} insecure={insecure} act={act} />)}</div>
    <OperationResult result={snapshot.operationResult} locale={locale} only="model" />
  </div>;
}

function ModelCard({ item, defaultId, busy, locale, privateNetwork, insecure, act }: { item: ModelAdmin; defaultId: string; busy: boolean; locale: Locale; privateNetwork: boolean; insecure: boolean; act: SectionProps["act"] }) {
  const c = copy(locale); const info = modelInfo(item.bundle.id, locale); const installed = ["installed", "ready", "available", "verified"].includes(item.status.state.toLowerCase());
  return <article className="card admin-entity-card"><div className="entity-card-heading"><div className="entity-icon"><HardDrive size={18} /></div><div><h3>{info.name}</h3><p>{info.description}</p></div><StatusBadge tone={installed ? "ok" : "neutral"}>{installed ? c.installed : c.notInstalled}</StatusBadge></div>
    {item.bundle.id === defaultId && <p className="admin-note"><Sparkles size={15} />{c.defaultModel}</p>}
    <div className="admin-form-actions">{installed ? <button className="secondary" disabled={busy} type="button" onClick={() => void act({ schemaVersion: 1, action: "model.verify", target: item.bundle.id }, { success: c.success })}>{c.verify}</button> : <button disabled={busy} type="button" onClick={() => void act({ schemaVersion: 1, action: "model.install", target: item.bundle.id, allowPrivateNetwork: privateNetwork, insecure }, { network: true, dangerous: privateNetwork || insecure, success: c.success })}><Download size={17} />{c.install}</button>}</div>
    <details className="admin-advanced"><summary>{c.technical}</summary><p className="breakable"><code>{item.bundle.id}</code></p><div className="task-actions"><button className="secondary" disabled={busy} type="button" onClick={() => void act({ schemaVersion: 1, action: "model.show", target: item.bundle.id }, { success: c.success })}>{c.show}</button><button className="secondary" disabled={busy} type="button" onClick={() => void act({ schemaVersion: 1, action: "model.path", target: item.bundle.id }, { success: c.success })}>{c.path}</button>{installed && <button className="danger" disabled={busy} type="button" onClick={() => void act({ schemaVersion: 1, action: "model.remove", target: item.bundle.id }, { dangerous: true, confirm: c.modelRemoveConfirm, success: c.success })}>{c.remove}</button>}</div></details>
  </article>;
}

function ProvidersSection({ snapshot, busy, locale, act }: SectionProps) {
  const c = copy(locale); const [open, setOpen] = useState(false); const [name, setName] = useState(""); const [url, setUrl] = useState(""); const [model, setModel] = useState(""); const [env, setEnv] = useState(""); const [capabilities, setCapabilities] = useState(""); const [timeout, setTimeoutValue] = useState(""); const [scope, setScope] = useState<"global" | "project">("project"); const [hosts, setHosts] = useState(""); const [privateNetwork, setPrivateNetwork] = useState(false);
  const effective = snapshot.providers.filter((item) => item.effective); const validTimeout = timeout === "" || /^[1-9][0-9]{0,7}$/.test(timeout) && Number(timeout) <= 86_400_000;
  return <div className="admin-section-stack"><SectionTitle icon={<Cloud />} title={c.providersTitle} body={c.providersBody} action={<button type="button" disabled={snapshot.configurationReadOnly} onClick={() => setOpen((value) => !value)}><Plus size={17} />{open ? c.cancel : c.addProvider}</button>} />
    {open && <article className="card admin-tool-card"><h2>{c.addProvider}</h2><div className="admin-form-grid"><Field label={c.serviceName}><input value={name} maxLength={128} onChange={(event) => setName(event.target.value)} /></Field><Field label={c.baseUrl}><input value={url} maxLength={4096} placeholder="https://dashscope.aliyuncs.com/compatible-mode/v1" onChange={(event) => setUrl(event.target.value)} /></Field><Field label={c.model}><input value={model} maxLength={256} onChange={(event) => setModel(event.target.value)} /></Field><Field label={c.apiKeyEnv} hint={c.apiKeyHint}><input value={env} maxLength={128} placeholder="DASHSCOPE_API_KEY" onChange={(event) => setEnv(event.target.value)} /></Field></div>
      <details className="admin-advanced"><summary>{c.advanced}</summary><div className="admin-form-grid"><Field label={c.capabilities}><input value={capabilities} maxLength={1024} onChange={(event) => setCapabilities(event.target.value)} /></Field><Field label={c.timeout}><input value={timeout} inputMode="numeric" maxLength={8} aria-invalid={!validTimeout} onChange={(event) => setTimeoutValue(event.target.value)} /></Field><Field label={c.hosts}><input value={hosts} maxLength={4096} onChange={(event) => setHosts(event.target.value)} /></Field><Field label={c.scope}><ScopeSelect value={scope} onChange={setScope} locale={locale} /></Field><CheckField label={c.privateNetwork} checked={privateNetwork} setChecked={setPrivateNetwork} /></div></details>
      <div className="admin-form-actions"><button disabled={busy || !name || !url || !model || !env || !validTimeout} type="button" onClick={() => void act({ schemaVersion: 1, action: "provider.add", scope, target: name, source: url, providerType: "openai-compatible", model, apiKeyEnv: env, capabilities: csv(capabilities), ...(timeout ? { timeoutMs: Number(timeout) } : {}) }, { dangerous: true, success: c.providerAddedSuccess })}>{c.add}</button></div></article>}
    {effective.length === 0 ? <EmptyState icon={<Cloud />} title={c.noProviders} body={c.noProvidersBody} /> : <div className="admin-grid">{effective.map((item) => <ProviderCard key={`${item.scope}:${item.name}`} item={item} all={snapshot.providers} busy={busy} locale={locale} hosts={hosts} privateNetwork={privateNetwork} readOnly={snapshot.configurationReadOnly} act={act} />)}</div>}
    <OperationResult result={snapshot.operationResult} locale={locale} only="providerTest" />
  </div>;
}

function ProviderCard({ item, all, busy, locale, hosts, privateNetwork, readOnly, act }: { item: ProviderAdmin; all: ProviderAdmin[]; busy: boolean; locale: Locale; hosts: string; privateNetwork: boolean; readOnly: boolean; act: SectionProps["act"] }) {
  const c = copy(locale); const layers = all.filter((candidate) => candidate.name === item.name && candidate.scope !== "effective" && candidate.actionScope);
  return <article className="card admin-entity-card"><div className="entity-card-heading"><div className="entity-icon"><Cloud size={18} /></div><div><h3>{item.name}</h3><p>{item.model ?? c.inherited}</p></div>{item.default ? <StatusBadge tone="ok">{c.defaultModel}</StatusBadge> : <StatusBadge tone={item.environmentSet === false ? "warning" : "neutral"}>{item.environmentSet === false ? c.environmentMissing : c.environmentReady}</StatusBadge>}</div>
    <p className="admin-endpoint breakable">{item.baseUrl ?? c.inherited}</p><div className="chip-row">{item.capabilities.map((value) => <span className="status-pill" key={value}>{value}</span>)}</div>
    <div className="admin-form-actions"><button disabled={busy || readOnly || !item.actionScope} type="button" onClick={() => void act({ schemaVersion: 1, action: "provider.test", scope: item.actionScope, target: item.name, allowHosts: csv(hosts), allowPrivateNetwork: privateNetwork }, { network: true, dangerous: privateNetwork, success: c.providerTestSuccess })}>{c.test}</button>{!item.default && item.actionScope && <button className="secondary" disabled={busy || readOnly} type="button" onClick={() => void act({ schemaVersion: 1, action: "provider.set-default", scope: item.actionScope, target: item.name }, { dangerous: true, success: c.providerDefaultSuccess })}>{c.setDefault}</button>}</div>
    <details className="admin-advanced"><summary>{c.technical}</summary><dl className="admin-detail-list"><Detail label={c.apiKeyEnv} value={item.apiKeyEnv ?? c.inherited} /><Detail label={c.timeout} value={item.timeoutMs ? String(item.timeoutMs) : c.inherited} /></dl>{layers.length > 0 && <ul className="admin-layer-list">{layers.map((layer) => <li key={layer.scope}><span>{friendlyScope(layer.scope, locale)}</span><button className="danger" disabled={busy || readOnly} type="button" onClick={() => void act({ schemaVersion: 1, action: "provider.remove", scope: layer.actionScope, target: item.name }, { dangerous: true, confirm: c.providerRemoveConfirm, success: c.success })}>{c.remove}</button></li>)}</ul>}</details>
  </article>;
}

function PluginsSection({ snapshot, busy, locale, act }: SectionProps) {
  const c = copy(locale); const [open, setOpen] = useState(false); const [source, setSource] = useState(""); const [sha, setSha] = useState(""); const [signer, setSigner] = useState(""); const [fingerprint, setFingerprint] = useState(""); const [scope, setScope] = useState<"global" | "project">("project");
  const effective = snapshot.plugins.filter((item) => item.effective);
  return <div className="admin-section-stack"><SectionTitle icon={<Package />} title={c.pluginsTitle} body={c.pluginsBody} action={<button type="button" disabled={snapshot.configurationReadOnly} onClick={() => setOpen((value) => !value)}><Plus size={17} />{open ? c.cancel : c.addPlugin}</button>} />
    {open && <article className="card admin-tool-card"><h2>{c.addPlugin}</h2><Field label={c.packageSource}><input value={source} maxLength={4096} onChange={(event) => setSource(event.target.value)} /></Field><details className="admin-advanced"><summary>{c.advanced}</summary><div className="admin-form-grid"><Field label={c.sha}><input value={sha} maxLength={64} onChange={(event) => setSha(event.target.value)} /></Field><Field label={c.signer}><input value={signer} maxLength={128} onChange={(event) => setSigner(event.target.value)} /></Field><Field label={c.fingerprint}><input value={fingerprint} maxLength={64} onChange={(event) => setFingerprint(event.target.value)} /></Field><Field label={c.scope}><ScopeSelect value={scope} onChange={setScope} locale={locale} /></Field></div></details><div className="admin-form-actions"><button disabled={busy || !source} type="button" onClick={() => void act({ schemaVersion: 1, action: "plugin.install", scope, source, ...(sha ? { sha256: sha } : {}), ...(signer ? { signingKeyId: signer } : {}), ...(fingerprint ? { signingKeySha256: fingerprint } : {}) }, { dangerous: true, network: /^https:\/\//i.test(source), success: c.pluginInstallSuccess })}>{c.install}</button></div></article>}
    {effective.length === 0 ? <EmptyState icon={<Package />} title={c.noPlugins} body={c.noPluginsBody} /> : <div className="admin-grid">{effective.map((item) => <PluginCard key={`${item.scope}:${item.id}`} item={item} all={snapshot.plugins} busy={busy} locale={locale} readOnly={snapshot.configurationReadOnly} act={act} />)}</div>}
  </div>;
}

function PluginCard({ item, all, busy, locale, readOnly, act }: { item: PluginAdmin; all: PluginAdmin[]; busy: boolean; locale: Locale; readOnly: boolean; act: SectionProps["act"] }) {
  const c = copy(locale); const layers = all.filter((candidate) => candidate.id === item.id && candidate.scope !== "effective" && candidate.actionScope);
  return <article className="card admin-entity-card"><div className="entity-card-heading"><div className="entity-icon"><Package size={18} /></div><div><h3>{item.id}</h3><p>{item.version ?? item.protocol ?? c.inherited}</p></div><StatusBadge tone={item.enabled === false ? "neutral" : "ok"}>{item.enabled === false ? c.disabled : c.enabled}</StatusBadge></div>
    <div className="admin-form-actions"><button className="secondary" disabled={busy || readOnly || !item.actionScope} type="button" onClick={() => void act({ schemaVersion: 1, action: "plugin.verify", scope: item.actionScope, target: item.id }, { success: c.pluginVerifySuccess })}>{c.verify}</button></div>
    <details className="admin-advanced"><summary>{c.technical}</summary><dl className="admin-detail-list"><Detail label={c.packageSource} value={item.source ?? c.inherited} /><Detail label={c.target} value={item.target ?? c.inherited} /><Detail label={c.protocol} value={item.protocol ?? c.inherited} /><Detail label={c.verification} value={item.verification ?? c.inherited} /><Detail label={c.sha} value={item.sha256 ?? c.inherited} /><Detail label={c.signer} value={item.signingKeyId ?? c.inherited} /></dl>{layers.length > 0 && <ul className="admin-layer-list">{layers.map((layer) => <li key={layer.scope}><span>{friendlyScope(layer.scope, locale)}</span><div className="task-actions"><button className="secondary" disabled={busy || readOnly} type="button" onClick={() => void act({ schemaVersion: 1, action: layer.enabled === false ? "plugin.enable" : "plugin.disable", scope: layer.actionScope, target: item.id }, { dangerous: layer.enabled === false, success: c.success })}>{layer.enabled === false ? c.enable : c.disable}</button><button className="danger" disabled={busy || readOnly} type="button" onClick={() => void act({ schemaVersion: 1, action: "plugin.remove", scope: layer.actionScope, target: item.id }, { dangerous: true, confirm: c.pluginRemoveConfirm, success: c.success })}>{c.remove}</button></div></li>)}</ul>}</details>
  </article>;
}

function ConfigurationSection({ snapshot, busy, locale, act }: SectionProps) {
  const c = copy(locale); const [key, setKey] = useState("conversion.ocr.policy"); const [value, setValue] = useState("auto"); const [scope, setScope] = useState<"global" | "project">("project"); const [prompt, setPrompt] = useState(""); const [profile, setProfile] = useState(""); const [from, setFrom] = useState(""); const [source, setSource] = useState(""); const [resolved, setResolved] = useState(false); const [force, setForce] = useState(false);
  return <div className="admin-section-stack"><SectionTitle icon={<Settings2 />} title={c.configTitle} body={c.configBody} />
    <div className="admin-grid admin-config-grid"><article className="card admin-tool-card"><h2>{c.chooseSetting}</h2><Field label={c.chooseSetting}><select value={key} onChange={(event) => setKey(event.target.value)}>{adminConfigKeys.map((item) => <option key={item} value={item}>{friendlyConfigKey(item, locale)}</option>)}</select></Field><Field label={c.value}><input value={value} maxLength={4096} onChange={(event) => setValue(event.target.value)} /></Field><Field label={c.scope}><ScopeSelect value={scope} onChange={setScope} locale={locale} /></Field><div className="admin-form-actions"><button className="secondary" disabled={busy || !key} type="button" onClick={() => void act({ schemaVersion: 1, action: "config.get", target: key }, { success: c.success })}>{c.readCurrent}</button><button disabled={busy || snapshot.configurationReadOnly || !key} type="button" onClick={() => void act({ schemaVersion: 1, action: "config.set", scope, target: key, value }, { dangerous: true, success: c.configSaved })}>{c.save}</button><button className="secondary" disabled={busy || snapshot.configurationReadOnly || !key} type="button" onClick={() => void act({ schemaVersion: 1, action: "config.unset", scope, target: key }, { dangerous: true, success: c.configRestored })}>{c.restore}</button></div><details className="admin-advanced"><summary>{c.technical}</summary><code className="breakable">{key}</code><div className="admin-form-row"><Field label={c.promptName}><input value={prompt} pattern="[A-Za-z0-9_-]+" maxLength={128} onChange={(event) => setPrompt(event.target.value)} /></Field><button className="secondary" disabled={!/^[A-Za-z0-9_-]{1,128}$/.test(prompt)} type="button" onClick={() => setKey(`conversion.ai.prompts.${prompt}`)}>{c.addPrompt}</button></div></details></article>
      <article className="card admin-tool-card"><h2>{c.profiles}</h2>{snapshot.profiles.length === 0 ? <p className="muted">{c.noProfiles}</p> : <ul className="admin-simple-list">{snapshot.profiles.map((item) => <li key={`${item.scope}:${item.name}`}><div><strong>{item.name}</strong><small>{friendlyScope(item.scope, locale)}{item.active ? ` · ${c.effective}` : ""}</small></div><div className="task-actions"><button className="secondary" disabled={busy} type="button" onClick={() => void act({ schemaVersion: 1, action: "profile.show", scope: item.scope === "global" ? "global" : "project", target: item.name }, { success: c.success })}>{c.show}</button>{item.scope !== "effective" && <button className="danger" disabled={busy || snapshot.configurationReadOnly} type="button" onClick={() => void act({ schemaVersion: 1, action: "profile.remove", scope: item.scope === "global" ? "global" : "project", target: item.name }, { dangerous: true, confirm: c.profileRemoveConfirm, success: c.success })}>{c.remove}</button>}</div></li>)}</ul>}<Field label={c.newProfile}><input value={profile} maxLength={128} onChange={(event) => setProfile(event.target.value)} /></Field><Field label={c.copyFrom}><input value={from} maxLength={128} onChange={(event) => setFrom(event.target.value)} /></Field><div className="admin-form-actions"><button disabled={busy || snapshot.configurationReadOnly || !/^[A-Za-z0-9_-]{1,128}$/.test(profile)} type="button" onClick={() => void act({ schemaVersion: 1, action: "profile.create", scope, target: profile, ...(from ? { from } : {}) }, { dangerous: true, success: c.profileCreated })}>{c.create}</button></div></article></div>
    <article className="card admin-tool-card"><h2>{c.configTools}</h2><p>{c.configToolsBody}</p><details className="admin-advanced"><summary>{c.advanced}</summary><div className="admin-form-grid"><Field label={c.validationPath}><input value={source} maxLength={4096} onChange={(event) => setSource(event.target.value)} /></Field><CheckField label={c.resolved} checked={resolved} setChecked={setResolved} /><CheckField label={c.force} checked={force} setChecked={setForce} /></div></details><div className="task-actions"><button className="secondary" disabled={busy} type="button" onClick={() => void act({ schemaVersion: 1, action: "config.paths" }, { success: c.success })}>{c.paths}</button><button className="secondary" disabled={busy} type="button" onClick={() => void act({ schemaVersion: 1, action: "config.validate", ...(source ? { source } : {}) }, { success: c.success })}>{c.validate}</button><button className="secondary" disabled={busy} type="button" onClick={() => void act({ schemaVersion: 1, action: "config.show", resolved }, { success: c.success })}>{c.show}</button><button disabled={busy || snapshot.configurationReadOnly} type="button" onClick={() => void act({ schemaVersion: 1, action: "config.init", scope, force }, { dangerous: true, success: c.success })}>{c.initialize}</button></div></article>
    <OperationResult result={snapshot.operationResult} locale={locale} />
    <details className="card admin-advanced admin-raw-config"><summary>{c.rawConfig}</summary><pre className="config-json">{JSON.stringify(snapshot.configuration, null, 2)}</pre></details>
  </div>;
}

function DoctorSection({ snapshot, busy, locale, act }: SectionProps) {
  const c = copy(locale); const [network, setNetwork] = useState(false); const checks = snapshot.operationResult?.kind === "doctor" ? snapshot.operationResult.checks : snapshot.doctor; const issues = checks.filter((item) => !isHealthy(item) && !isSkipped(item)); const passed = checks.filter(isHealthy); const skipped = checks.filter(isSkipped);
  return <div className="admin-section-stack"><SectionTitle icon={<Wrench />} title={c.doctorTitle} body={c.doctorBody} action={<button disabled={busy} type="button" onClick={() => void act({ schemaVersion: 1, action: "doctor.run", allowPrivateNetwork: false }, { network, success: c.doctorDone })}><Activity size={17} />{c.run}</button>} />
    <label className="admin-doctor-network"><input type="checkbox" checked={network} onChange={(event) => setNetwork(event.target.checked)} />{c.checkNetwork}</label>
    <div className={`card doctor-summary ${issues.length === 0 ? "healthy" : "attention"}`}>{issues.length === 0 ? <CheckCircle2 size={30} /> : <CircleAlert size={30} />}<div><h2>{issues.length === 0 ? c.healthy : `${issues.length} ${c.attention}`}</h2><p>{issues.length === 0 ? c.healthyBody : `${passed.length} ${c.passed}${skipped.length ? ` · ${skipped.length} ${c.notRun}` : ""}`}</p></div></div>
    {issues.length > 0 && <div className="doctor-list">{issues.map((item) => <DoctorCard key={item.id} item={item} locale={locale} />)}</div>}
    {passed.length > 0 && <details className="card admin-advanced doctor-passed"><summary>{c.passedChecks}（{passed.length}）</summary><div className="doctor-list">{passed.map((item) => <DoctorCard key={item.id} item={item} locale={locale} />)}</div></details>}
    {skipped.length > 0 && <details className="card admin-advanced doctor-passed"><summary>{c.skippedChecks}（{skipped.length}）</summary><div className="doctor-list">{skipped.map((item) => <DoctorCard key={item.id} item={item} locale={locale} />)}</div></details>}
  </div>;
}

function DoctorCard({ item, locale }: { item: DoctorAdmin; locale: Locale }) {
  const c = copy(locale); const info = doctorInfo(item, locale); const healthy = isHealthy(item); const skipped = isSkipped(item);
  return <article className="card doctor-card"><div className="doctor-card-heading">{healthy ? <CheckCircle2 size={20} /> : <CircleAlert size={20} />}<div><h3>{info.title}</h3><StatusBadge tone={healthy ? "ok" : skipped ? "neutral" : "warning"}>{friendlyStatus(item.status, locale)}</StatusBadge></div></div>{!healthy && !skipped && <div className="doctor-guidance"><div><strong>{c.impact}</strong><p>{info.impact}</p></div><div><strong>{c.nextStep}</strong><p>{info.action}</p></div></div>}<details className="admin-advanced"><summary>{c.technical}</summary><p><code>{item.id}</code></p><p className="breakable">{item.detail}</p></details></article>;
}

function SectionTitle({ icon, title, body, action }: { icon: ReactNode; title: string; body: string; action?: ReactNode }) { return <header className="admin-section-title"><div className="admin-section-icon">{icon}</div><div><h2>{title}</h2><p>{body}</p></div>{action && <div className="admin-section-action">{action}</div>}</header>; }
function Field({ label, hint, children }: { label: string; hint?: string; children: ReactNode }) { return <label className="admin-field"><span>{label}</span>{children}{hint && <small>{hint}</small>}</label>; }
function CheckField({ label, checked, setChecked }: { label: string; checked: boolean; setChecked: (value: boolean) => void }) { return <label className="check admin-check"><input type="checkbox" checked={checked} onChange={(event) => setChecked(event.target.checked)} /><span>{label}</span></label>; }
function ScopeSelect({ value, onChange, locale }: { value: "global" | "project"; onChange: (value: "global" | "project") => void; locale: Locale }) { const c = copy(locale); return <select value={value} onChange={(event) => onChange(event.target.value === "global" ? "global" : "project")}><option value="project">{c.project}</option><option value="global">{c.global}</option></select>; }
function Feedback({ kind, children }: { kind: "error" | "success" | "readonly"; children: ReactNode }) { return <div className={`admin-feedback ${kind}`} role={kind === "error" ? "alert" : "status"}>{children}</div>; }
function StatusBadge({ tone, children }: { tone: "ok" | "warning" | "neutral"; children: ReactNode }) { return <span className={`admin-status ${tone}`}>{children}</span>; }
function Detail({ label, value }: { label: string; value: string }) { return <div><dt>{label}</dt><dd className="breakable">{value}</dd></div>; }
function EmptyState({ icon, title, body }: { icon: ReactNode; title: string; body: string }) { return <article className="card admin-empty"><div className="entity-icon">{icon}</div><h2>{title}</h2><p>{body}</p></article>; }

function OperationResult({ result, locale, only }: { result: AdminOperationResult | undefined; locale: Locale; only?: AdminOperationResult["kind"] }) {
  const c = copy(locale); if (!result || only && result.kind !== only) return null;
  if (result.kind === "detection") return <div className="admin-result"><h3>{c.detectionResult}</h3>{result.candidates.length === 0 ? <p>—</p> : result.candidates.slice(0, 3).map((item) => <div className="admin-result-row" key={`${item.format}:${item.detectorId}`}><strong>{item.format}</strong><span>{c.confidence} {Math.round(item.confidence * 100)}%</span></div>)}</div>;
  if (result.kind === "providerTest") return <div className="admin-result"><h3>{c.providerResult}</h3><div className="admin-result-row"><strong>{result.configuredModelAvailable ? c.ready : c.unavailable}</strong><span>{result.modelCount} {c.availableModels.toLowerCase()}</span></div></div>;
  return <details className="card admin-advanced"><summary>{c.result}</summary><pre className="config-json">{JSON.stringify(result, null, 2)}</pre></details>;
}

function csv(value: string) { return value.split(",").map((item) => item.trim()).filter(Boolean); }
function errorCode(reason: unknown) { return reason instanceof Error && "code" in reason ? String((reason as { code: unknown }).code) : "requestFailed"; }
function friendlyError(code: string, locale: Locale) { const chinese = locale === "zh-CN"; const known: Record<string, [string, string]> = { unreachable: ["无法连接本地服务，请确认 into-md 仍在运行。", "Cannot reach the local service. Make sure into-md is still running."], requestFailed: ["操作没有完成，请重试。", "The operation did not complete. Try again."], authorizationRequired: ["本次操作需要重新确认，请再试一次。", "This action needs fresh confirmation. Try again."], invalidAction: ["当前输入无法执行，请检查后重试。", "The current input cannot be used. Check it and try again."], configurationReadOnly: ["当前为只读模式，不能修改设置。", "Settings cannot be changed in view-only mode."] }; return (known[code] ?? [chinese ? `操作失败（${code}）。` : `Operation failed (${code}).`, chinese ? `操作失败（${code}）。` : `Operation failed (${code}).`])[chinese ? 0 : 1]; }
function friendlyFamily(value: string, locale: Locale) { const zhNames: Record<string, string> = { document: "文档", text: "文本", image: "图片", audio: "音频", video: "视频", archive: "压缩包", data: "数据", presentation: "演示文稿", spreadsheet: "表格" }; return locale === "zh-CN" ? zhNames[value.toLowerCase()] ?? value : value; }
function friendlyScope(value: string, locale: Locale) { const c = copy(locale); return value === "global" ? c.global : value === "project" ? c.project : c.effective; }
function friendlyStatus(value: string, locale: Locale) { if (value.toLowerCase() === "skipped") return locale === "zh-CN" ? "未检查" : "Not checked"; const healthy = ["ok", "pass", "passed", "ready", "healthy", "available"].includes(value.toLowerCase()); return healthy ? copy(locale).ready : locale === "zh-CN" ? "需要处理" : "Needs attention"; }
function isHealthy(item: DoctorAdmin) { return ["ok", "pass", "passed", "ready", "healthy", "available"].includes(item.status.toLowerCase()); }
function isSkipped(item: DoctorAdmin) { return item.status.toLowerCase() === "skipped"; }
function modelInfo(id: string, locale: Locale) { const chinese = locale === "zh-CN"; const normalized = id.toLowerCase(); if (normalized.includes("whisper")) return { name: chinese ? "多语言语音识别" : "Multilingual transcription", description: chinese ? "将音频和视频转换为文字" : "Turn audio and video into text" }; if (normalized.includes("speaker") || normalized.includes("silero") || normalized.includes("diar")) return { name: chinese ? "说话人区分" : "Speaker identification", description: chinese ? "区分会议中的不同发言人" : "Separate speakers in meetings" }; if (normalized.includes("detector")) return { name: chinese ? "文字区域检测" : "Text region detection", description: chinese ? "找到扫描件和图片中文字所在的位置" : "Locate text regions in scans and images" }; if (normalized.includes("recognizer")) return { name: chinese ? "文字内容识别" : "Text recognition", description: chinese ? "读取已定位区域中的中英文内容" : "Read Chinese and English text from located regions" }; if (normalized.includes("ocr") || normalized.includes("paddle") || normalized.includes("pp")) return { name: chinese ? "中英文图片文字识别" : "Chinese and English OCR", description: chinese ? "完整识别扫描件和图片中的文字" : "Complete text recognition for scans and images" }; return { name: id, description: chinese ? "本地转换组件" : "Local conversion component" }; }
function friendlyConfigKey(key: string, locale: Locale) { const chinese = locale === "zh-CN"; const names: Record<string, [string, string]> = { "conversion.ocr.policy": ["扫描件文字识别", "OCR behavior"], "conversion.ocr.languages": ["OCR 识别语言", "OCR languages"], "conversion.asr.language": ["语音识别语言", "Transcription language"], "conversion.asr.chinese_script": ["中文输出字形", "Chinese output script"], "conversion.timeout_ms": ["转换超时时间", "Conversion timeout"], "conversion.ai.provider": ["默认 AI 服务", "Default AI service"], "conversion.ai.model": ["默认 AI 模型", "Default AI model"], "conversion.output.asset_mode": ["图片等资源的保存方式", "Asset handling"], "conversion.output.include_provenance": ["保留来源信息", "Include provenance"], "cli.language": ["界面语言", "Interface language"], "cli.jobs": ["并行任务数", "Concurrent jobs"] }; const name = names[key]?.[chinese ? 0 : 1] ?? key.split(".").slice(-2).join(" · "); return `${name} — ${key}`; }
function doctorInfo(item: DoctorAdmin, locale: Locale) { const chinese = locale === "zh-CN"; const id = item.id.toLowerCase(); if (id === "runtime.pdfium") return { title: chinese ? "PDF 解析组件" : "PDF parser", impact: chinese ? "PDF 文件可能无法转换。" : "PDF files may not convert.", action: chinese ? "安装随完整包提供的 PDF 组件，然后重新检查。" : "Install the PDF component included with the complete package, then run diagnostics again." }; if (id === "runtime.ocr") return { title: chinese ? "图片文字识别组件" : "Image text recognition", impact: chinese ? "扫描件和图片中的文字无法识别。" : "Text in scans and images cannot be recognized.", action: chinese ? "运行 `into-md setup ocr` 安装并验证 OCR 组件。" : "Run `into-md setup ocr` to install and verify OCR." }; if (id === "runtime.legacy-office") return { title: chinese ? "旧版 Office 文档组件" : "Legacy Office documents", impact: chinese ? "旧版 Word、Excel 或 PowerPoint 文件可能无法转换。" : "Older Word, Excel, or PowerPoint files may not convert.", action: chinese ? "安装完整包中与当前平台匹配的旧版 Office 组件。" : "Install the platform-matched legacy Office component from the complete package." }; if (id === "runtime.asr") return { title: chinese ? "音频转写组件" : "Audio transcription", impact: chinese ? "音频和视频无法转写为文字。" : "Audio and video cannot be transcribed.", action: chinese ? "运行 `into-md setup media` 安装并验证音频组件。" : "Run `into-md setup media` to install and verify media components." }; if (id === "runtime.diarization") return { title: chinese ? "说话人区分组件" : "Speaker identification", impact: chinese ? "会议转写可以生成文字，但无法区分不同发言人。" : "Meetings can be transcribed, but speakers cannot be separated.", action: chinese ? "使用包含 ONNX Runtime 的完整包，再运行 `into-md setup media`。" : "Use the complete package with ONNX Runtime, then run `into-md setup media`." }; if (id === "modelfiles") return { title: chinese ? "模型下载能力" : "Model downloads", impact: chinese ? "需要时可能无法直接下载本地模型。" : "Local models may not download when needed.", action: chinese ? "勾选“同时检查联网能力”后重新检查；已离线安装模型时可忽略。" : "Enable the network check and run again; ignore this when models were installed offline." }; if (id.includes("model")) return { title: chinese ? "本地模型清单" : "Local model catalog", impact: chinese ? "相关的 OCR 或语音能力可能无法使用。" : "Related OCR or transcription features may be unavailable.", action: chinese ? "前往“本地模型”安装或验证所需模型。" : "Open Local models to install or verify the required model." }; if (id.includes("provider") || id.includes("api")) return { title: chinese ? "AI 服务连接" : "AI service connection", impact: chinese ? "需要云端 AI 的增强功能无法运行。" : "Cloud AI enhancements cannot run.", action: chinese ? "前往“AI 服务”检查地址、模型和密钥环境变量。" : "Check the address, model, and key environment variable under AI services." }; if (id.includes("plugin")) return { title: chinese ? "扩展插件" : "Extensions", impact: chinese ? "部分额外格式或能力可能不可用。" : "Some extra formats or capabilities may be unavailable.", action: chinese ? "前往“扩展插件”验证对应插件。" : "Open Extensions and verify the affected plugin." }; if (id.includes("config")) return { title: chinese ? "配置文件" : "Configuration", impact: chinese ? "部分设置可能没有生效。" : "Some settings may not be applied.", action: chinese ? "前往“设置”验证配置文件。" : "Open Settings and validate the configuration file." }; if (id.includes("network")) return { title: chinese ? "网络访问检查" : "Network check", impact: chinese ? "本次没有检查联网能力。" : "Network access was not checked this time.", action: chinese ? "需要下载模型或连接 AI 服务时，勾选联网检查后重试。" : "Enable the network check when downloading models or using AI services." }; return { title: item.id.replaceAll(/[._-]+/g, " "), impact: chinese ? "相关能力可能无法正常工作。" : "The related capability may not work correctly.", action: chinese ? "展开技术详情查看原因，修复后重新检查。" : "Open technical details, address the cause, and run diagnostics again." }; }
