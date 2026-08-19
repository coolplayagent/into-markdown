import { useEffect, useRef, useState } from "react";
import type { AdminAction, AdminSnapshot, ApiClient } from "./api";
import { RouteLink } from "./router";
import { useI18n } from "./i18n";

export type AdminSection = "formats" | "models" | "providers" | "plugins" | "configuration" | "doctor";
export const adminSections: AdminSection[] = ["formats", "models", "providers", "plugins", "configuration", "doctor"];
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

export function AdminPage({ api, section }: { api: ApiClient; section: AdminSection }) {
  const { t } = useI18n();
  const [snapshot, setSnapshot] = useState<AdminSnapshot | null>(null);
  const [error, setError] = useState("");
  const [attempt, setAttempt] = useState(0);
  const [busy, setBusy] = useState(false);
  const [dangerous, setDangerous] = useState(false);
  const [network, setNetwork] = useState(false);
  const [configKey, setConfigKey] = useState("conversion.ocr.policy");
  const [configValue, setConfigValue] = useState("auto");
  const [configSource, setConfigSource] = useState("");
  const [profile, setProfile] = useState("");
  const [pluginSource, setPluginSource] = useState("");
  const [pluginSha, setPluginSha] = useState("");
  const [signerId, setSignerId] = useState("");
  const [signerFingerprint, setSignerFingerprint] = useState("");
  const [installScope, setInstallScope] = useState<"global" | "project">("project");
  const [formatSource, setFormatSource] = useState("");
  const [formatCharset, setFormatCharset] = useState("");
  const [formatHint, setFormatHint] = useState("");
  const [formatExtension, setFormatExtension] = useState("");
  const [formatMime, setFormatMime] = useState("");
  const [allowHosts, setAllowHosts] = useState("");
  const [privateNetwork, setPrivateNetwork] = useState(false);
  const [insecure, setInsecure] = useState(false);
  const [force, setForce] = useState(false);
  const [configResolved, setConfigResolved] = useState(false);
  const [promptName, setPromptName] = useState("");
  const [providerName, setProviderName] = useState("");
  const [providerUrl, setProviderUrl] = useState("");
  const [providerModel, setProviderModel] = useState("");
  const [providerEnv, setProviderEnv] = useState("");
  const [providerCapabilities, setProviderCapabilities] = useState("");
  const [providerTimeout, setProviderTimeout] = useState("");
  const [editScope, setEditScope] = useState<"global" | "project">("project");
  const [profileFrom, setProfileFrom] = useState("");
  const providerTimeoutValid = providerTimeout === ""
    || (/^[1-9][0-9]{0,7}$/.test(providerTimeout) && Number(providerTimeout) <= 86_400_000);
  const actionInFlight = useRef(false);
  useEffect(() => {
    setNetwork(false); setDangerous(false); setPrivateNetwork(false); setInsecure(false); setForce(false); setAllowHosts("");
  }, [section]);
  useEffect(() => {
    const controller = new AbortController(); setError("");
    void api.admin(controller.signal).then(setSnapshot, (reason: unknown) => {
      if (!controller.signal.aborted) setError(reason instanceof Error && "code" in reason ? String((reason as { code: unknown }).code) : "requestFailed");
    });
    return () => controller.abort();
  }, [api, attempt]);
  const act = async (action: AdminAction) => {
    if (actionInFlight.current) return;
    actionInFlight.current = true;
    setBusy(true); setError("");
    const requested = { ...action, schemaVersion: 1 as const, authorizeDangerous: dangerous, authorizeNetwork: network };
    setDangerous(false); setNetwork(false); setPrivateNetwork(false); setInsecure(false); setForce(false); setAllowHosts("");
    try {
      const authorizationGrant = dangerous || network ? await api.adminGrant(requested) : undefined;
      const outcome = await api.adminAction({ ...requested, ...(authorizationGrant ? { authorizationGrant } : {}) });
      if (outcome.operationResult) {
        const operationResult = outcome.operationResult;
        setSnapshot((current) => current ? { ...current, operationResult } : current);
      } else {
        try { setSnapshot(await api.admin()); }
        catch { setError("actionSucceededRefreshFailed"); }
      }
    } catch (reason) {
      setError(reason instanceof Error && "code" in reason ? String((reason as { code: unknown }).code) : "requestFailed");
    } finally { actionInFlight.current = false; setBusy(false); }
  };
  return <section className="admin-page">
    <div className="page-heading"><p className="eyebrow">{t("localAdministration")}</p><h1>{t("administration")}</h1><p>{t("administrationIntro")}</p></div>
    <nav className="admin-tabs" aria-label={t("administration")}>
      {adminSections.map((item) => <RouteLink key={item} href={`/admin/${item}`} className={section === item ? "active" : ""}>{t(item)}</RouteLink>)}
    </nav>
    {error && <div className="card admin-error" role="alert"><p>{error}</p><button type="button" onClick={() => setAttempt((value) => value + 1)}>{t("retry")}</button></div>}
    {!snapshot ? <p role="status">{t("loading")}</p> : <>
      {snapshot.configurationReadOnly && <div className="card" role="status"><p>{t("administrationReadOnly")}</p></div>}
      {section === "models" && snapshot.operationResult !== undefined && <pre className="card config-json">{JSON.stringify(snapshot.operationResult, null, 2)}</pre>}
      {section === "providers" && snapshot.operationResult !== undefined && <pre className="card config-json">{JSON.stringify(snapshot.operationResult, null, 2)}</pre>}
      {section === "providers" && <article className="card"><h2>{t("setDefault")}</h2><p>{providerName || "—"} · {editScope}</p><button disabled={busy || snapshot.configurationReadOnly || !providerName || !dangerous} type="button" onClick={() => void act({ schemaVersion: 1, action: "provider.set-default", scope: editScope, target: providerName })}>{t("setDefault")}</button></article>}
      {section === "formats" && <article className="card"><h2>{t("detectionAuthority")}</h2><label><span>{t("format")}</span><input value={formatHint} maxLength={64} onChange={(event) => setFormatHint(event.target.value)} /></label><label><span>{t("extension")}</span><input value={formatExtension} maxLength={32} onChange={(event) => setFormatExtension(event.target.value)} /></label><label><span>{t("mimeType")}</span><input value={formatMime} maxLength={128} onChange={(event) => setFormatMime(event.target.value)} /></label></article>}
      {section === "configuration" && <article className="card">
        <h2>{t("configurationManagement")}</h2>
        <label><span>{t("typedKey")}</span><select value={configKey} onChange={(event) => setConfigKey(event.target.value)}>{adminConfigKeys.map((key) => <option key={key} value={key}>{key}</option>)}</select></label>
        <label><span>{t("promptKeyName")}</span><input value={promptName} pattern="[A-Za-z0-9_-]+" maxLength={128} onChange={(event) => setPromptName(event.target.value)} /></label>
        <button disabled={busy || snapshot.configurationReadOnly || !/^[A-Za-z0-9_-]{1,128}$/.test(promptName)} type="button" onClick={() => setConfigKey(`conversion.ai.prompts.${promptName}`)}>{t("selectPromptKey")}</button>
        <label><span>{t("validationPath")}</span><input value={configSource} maxLength={4096} onChange={(event) => setConfigSource(event.target.value)} /></label>
        <label><input type="checkbox" checked={configResolved} onChange={(event) => setConfigResolved(event.target.checked)} />{t("resolvedConfig")}</label>
        <label><input type="checkbox" checked={force} onChange={(event) => setForce(event.target.checked)} />{t("forceOverwriteConfig")}</label>
        <div className="task-actions"><button disabled={busy || snapshot.configurationReadOnly} type="button" onClick={() => void act({ schemaVersion: 1, action: "config.paths" })}>{t("paths")}</button><button disabled={busy || snapshot.configurationReadOnly} type="button" onClick={() => void act({ schemaVersion: 1, action: "config.show", resolved: configResolved })}>{t("show")}</button><button disabled={busy || snapshot.configurationReadOnly || !configKey} type="button" onClick={() => void act({ schemaVersion: 1, action: "config.get", target: configKey })}>{t("get")}</button><button disabled={busy || snapshot.configurationReadOnly} type="button" onClick={() => void act({ schemaVersion: 1, action: "config.validate", ...(configSource ? { source: configSource } : {}) })}>{t("validate")}</button><button disabled={busy || snapshot.configurationReadOnly || !dangerous} type="button" onClick={() => void act({ schemaVersion: 1, action: "config.init", scope: editScope, force })}>{t("initialize")}</button></div>
      </article>}
      {section === "doctor" && <article className="card"><h2>{t("doctor")}</h2><button disabled={busy || privateNetwork && (!network || !dangerous)} type="button" onClick={() => void act({ schemaVersion: 1, action: "doctor.run", allowPrivateNetwork: privateNetwork })}>{t("runChecks")}</button>{snapshot.operationResult !== undefined && <pre className="config-json">{JSON.stringify(snapshot.operationResult, null, 2)}</pre>}</article>}
      {section === "formats" && <><article className="card"><h2>{t("detectFormat")}</h2><label><span>{t("localPath")}</span><input value={formatSource} maxLength={4096} onChange={(event) => setFormatSource(event.target.value)} /></label><label><span>{t("charset")}</span><input value={formatCharset} maxLength={128} onChange={(event) => setFormatCharset(event.target.value)} /></label><label><span>{t("allowHosts")}</span><input value={allowHosts} maxLength={4096} onChange={(event) => setAllowHosts(event.target.value)} /></label><button disabled={busy || !formatSource || privateNetwork && (!network || !dangerous)} type="button" onClick={() => void act({ schemaVersion: 1, action: "format.detect", source: formatSource, ...(formatCharset ? { charset: formatCharset } : {}), ...(formatHint ? { formatHint } : {}), ...(formatExtension ? { extension: formatExtension } : {}), ...(formatMime ? { mimeType: formatMime } : {}), allowHosts: allowHosts.split(",").map((value) => value.trim()).filter(Boolean), allowPrivateNetwork: privateNetwork })}>{t("detect")}</button>{snapshot.operationResult !== undefined && <pre className="config-json">{JSON.stringify(snapshot.operationResult, null, 2)}</pre>}</article><div className="admin-grid">{snapshot.formats.map((item) => <article className="card" key={item.format}><h2>{item.format}</h2><p>{item.family} · {item.status} · {item.source}</p><p>{item.extensions.join(", ")}</p>{item.runtimeComponent && <p>{item.runtimeComponent} · {item.installHint}</p>}</article>)}</div></>}
      {section === "models" && <div className="admin-grid">{snapshot.models.entries.map((item) => <article className="card" key={item.bundle.id}><h2>{item.bundle.id}</h2><p>{item.status.state} · {item.bundle.availability}</p><div className="task-actions"><button disabled={busy} type="button" onClick={() => void act({ schemaVersion: 1, action: "model.show", target: item.bundle.id })}>{t("show")}</button><button disabled={busy} type="button" onClick={() => void act({ schemaVersion: 1, action: "model.path", target: item.bundle.id })}>{t("path")}</button><button disabled={busy} type="button" onClick={() => void act({ schemaVersion: 1, action: "model.verify", target: item.bundle.id })}>{t("verify")}</button><button disabled={busy || !network || (privateNetwork || insecure) && !dangerous} type="button" onClick={() => void act({ schemaVersion: 1, action: "model.install", target: item.bundle.id, allowPrivateNetwork: privateNetwork, insecure })}>{t("install")}</button><button disabled={busy || !dangerous} type="button" onClick={() => void act({ schemaVersion: 1, action: "model.remove", target: item.bundle.id })}>{t("remove")}</button></div></article>)}</div>}
      {section === "providers" && <>
        <article className="card plugin-installer">
          <h2>{t("addProvider")}</h2>
          <label><span>{t("providerName")}</span><input value={providerName} maxLength={128} onChange={(event) => setProviderName(event.target.value)} /></label>
          <label><span>{t("baseUrl")}</span><input value={providerUrl} maxLength={4096} onChange={(event) => setProviderUrl(event.target.value)} /></label>
          <label><span>{t("model")}</span><input value={providerModel} maxLength={256} onChange={(event) => setProviderModel(event.target.value)} /></label>
          <label><span>{t("environmentName")}</span><input value={providerEnv} maxLength={128} onChange={(event) => setProviderEnv(event.target.value)} /></label>
          <label><span>{t("capabilities")}</span><input value={providerCapabilities} maxLength={1024} onChange={(event) => setProviderCapabilities(event.target.value)} /></label>
          <label><span>{t("timeoutMs")}</span><input value={providerTimeout} inputMode="numeric" maxLength={8} aria-invalid={!providerTimeoutValid} onChange={(event) => setProviderTimeout(event.target.value)} /></label>
          <label><span>{t("allowHosts")}</span><input value={allowHosts} maxLength={4096} onChange={(event) => setAllowHosts(event.target.value)} /></label>
          <label><span>{t("scope")}</span><select value={editScope} onChange={(event) => setEditScope(event.target.value === "global" ? "global" : "project")}><option value="project">project</option><option value="global">global</option></select></label>
          <button disabled={busy || snapshot.configurationReadOnly || !providerName || !providerUrl || !providerModel || !providerEnv || !providerTimeoutValid || !dangerous} type="button" onClick={() => void act({ schemaVersion: 1, action: "provider.add", scope: editScope, target: providerName, source: providerUrl, providerType: "openai-compatible", model: providerModel, apiKeyEnv: providerEnv, capabilities: providerCapabilities.split(",").map((value) => value.trim()).filter(Boolean), ...(providerTimeout ? { timeoutMs: Number(providerTimeout) } : {}) })}>{t("create")}</button>
        </article>
        <div className="admin-grid">{snapshot.providers.map((item) => <article className="card" key={`${item.scope}:${item.name}`}>
          <h2>{item.name}</h2>
          <p><span className="status-pill">{item.scope}</span> · {item.effective ? t("effective") : `${t("shadowedBy")} ${item.shadowedBy}`}</p>
          <p>{item.baseUrl ?? t("inherited")}</p>
          <p>{item.model ?? t("inherited")} · {item.environmentSet === undefined ? t("inherited") : item.environmentSet ? t("configured") : t("missing")}</p>
          <p>{item.capabilities.join(", ") || "—"}</p>
          <div className="task-actions">
            <button disabled={busy || snapshot.configurationReadOnly || !network || !item.effective || !item.actionScope || privateNetwork && !dangerous} type="button" onClick={() => void act({ schemaVersion: 1, action: "provider.test", scope: item.actionScope, target: item.name, allowHosts: allowHosts.split(",").map((value) => value.trim()).filter(Boolean), allowPrivateNetwork: privateNetwork })}>{t("testProvider")}</button>
            {item.scope !== "effective" && <button disabled={busy || snapshot.configurationReadOnly || !dangerous || !item.actionScope} type="button" onClick={() => void act({ schemaVersion: 1, action: "provider.remove", scope: item.actionScope, target: item.name })}>{t("remove")}</button>}
          </div>
        </article>)}</div>
      </>}
      {section === "plugins" && <>
        <article className="card plugin-installer"><h2>{t("installPlugin")}</h2><label><span>{t("packageSource")}</span><input value={pluginSource} maxLength={4096} onChange={(event) => setPluginSource(event.target.value)} /></label><label><span>SHA-256</span><input value={pluginSha} maxLength={64} onChange={(event) => setPluginSha(event.target.value)} /></label><label><span>{t("signerId")}</span><input value={signerId} maxLength={128} onChange={(event) => setSignerId(event.target.value)} /></label><label><span>{t("signerFingerprint")}</span><input value={signerFingerprint} maxLength={64} onChange={(event) => setSignerFingerprint(event.target.value)} /></label><label><span>{t("scope")}</span><select value={installScope} onChange={(event) => setInstallScope(event.target.value === "global" ? "global" : "project")}><option value="project">project</option><option value="global">global</option></select></label><button disabled={busy || snapshot.configurationReadOnly || !pluginSource || !dangerous || pluginSource.startsWith("https://") && !network} type="button" onClick={() => void act({ schemaVersion: 1, action: "plugin.install", scope: installScope, source: pluginSource, ...(pluginSha ? { sha256: pluginSha } : {}), ...(signerId ? { signingKeyId: signerId } : {}), ...(signerFingerprint ? { signingKeySha256: signerFingerprint } : {}) })}>{t("install")}</button></article>
        <div className="admin-grid">{snapshot.plugins.length === 0 ? <article className="card"><p>{t("noneConfigured")}</p></article> : snapshot.plugins.map((item) => <article className="card" key={`${item.scope}:${item.id}`}>
          <h2>{item.id}</h2><p><span className="status-pill">{item.scope}</span> · {item.protocol ?? t("inherited")} · {item.verification ?? t("inherited")} · {item.effective ? t("effective") : `${t("shadowedBy")} ${item.shadowedBy}`}</p>
          <p className="breakable">{item.source ?? t("inherited")}</p><dl><dt>{t("target")}</dt><dd>{item.target ?? t("inherited")}</dd><dt>{t("packageSha256")}</dt><dd className="breakable">{item.sha256 ?? t("inherited")}</dd><dt>{t("signerId")}</dt><dd>{item.signingKeyId ?? t("inherited")}</dd><dt>{t("signerFingerprint")}</dt><dd className="breakable">{item.signingKeySha256 ?? t("inherited")}</dd></dl>
          <div className="task-actions">
            <button disabled={busy || snapshot.configurationReadOnly || !item.effective || !item.actionScope || !item.packageScope} type="button" onClick={() => void act({ schemaVersion: 1, action: "plugin.verify", scope: item.actionScope, target: item.id })}>{t("verify")}</button>
            {item.scope !== "effective" && <button disabled={busy || snapshot.configurationReadOnly || !item.actionScope || !item.enabled && !dangerous} type="button" onClick={() => void act({ schemaVersion: 1, action: item.enabled ? "plugin.disable" : "plugin.enable", scope: item.actionScope, target: item.id })}>{item.enabled ? t("disable") : t("enable")}</button>}
            {item.scope !== "effective" && <button disabled={busy || snapshot.configurationReadOnly || !dangerous || !item.actionScope} type="button" onClick={() => void act({ schemaVersion: 1, action: "plugin.remove", scope: item.actionScope, target: item.id })}>{t("remove")}</button>}
          </div>
        </article>)}</div>
      </>}
      {section === "configuration" && <div className="admin-grid"><article className="card"><h2>{t("configuration")}</h2><p className="breakable">{configKey}</p><label><span>{t("configured")}</span><input value={configValue} maxLength={4096} onChange={(event) => setConfigValue(event.target.value)} /></label><label><span>{t("scope")}</span><select value={editScope} onChange={(event) => setEditScope(event.target.value === "global" ? "global" : "project")}><option value="project">project</option><option value="global">global</option></select></label><div className="task-actions"><button disabled={busy || snapshot.configurationReadOnly || !dangerous} type="button" onClick={() => void act({ schemaVersion: 1, action: "config.set", scope: editScope, target: configKey, value: configValue })}>{t("save")}</button><button disabled={busy || snapshot.configurationReadOnly || !dangerous} type="button" onClick={() => void act({ schemaVersion: 1, action: "config.unset", scope: editScope, target: configKey })}>{t("unset")}</button></div></article><article className="card"><h2>{t("profiles")}</h2>{snapshot.profiles.map((item) => <div key={`${item.scope}:${item.name}`}><p><span className="status-pill">{item.scope}</span> · {item.name} · {item.effective ? t("effective") : `${t("shadowedBy")} ${item.shadowedBy}`}</p><div className="task-actions"><button disabled={busy || snapshot.configurationReadOnly} type="button" onClick={() => void act({ schemaVersion: 1, action: "profile.show", scope: item.scope === "global" ? "global" : "project", target: item.name })}>{t("show")}</button><button disabled={busy || snapshot.configurationReadOnly || !dangerous} type="button" onClick={() => void act({ schemaVersion: 1, action: "profile.remove", scope: item.scope === "global" ? "global" : "project", target: item.name })}>{t("remove")}</button></div></div>)}<label><span>{t("profiles")}</span><input value={profile} maxLength={128} pattern="[A-Za-z0-9_-]+" onChange={(event) => setProfile(event.target.value)} /></label><label><span>{t("copyFrom")}</span><input value={profileFrom} maxLength={128} pattern="[A-Za-z0-9_-]*" onChange={(event) => setProfileFrom(event.target.value)} /></label><button disabled={busy || snapshot.configurationReadOnly || !profile || !dangerous} type="button" onClick={() => void act({ schemaVersion: 1, action: "profile.create", scope: editScope, target: profile, ...(profileFrom ? { from: profileFrom } : {}) })}>{t("create")}</button>{snapshot.operationResult !== undefined && <pre className="config-json">{JSON.stringify(snapshot.operationResult, null, 2)}</pre>}</article><article className="card"><h2>{t("redactedConfiguration")}</h2><pre className="config-json">{JSON.stringify(snapshot.configuration, null, 2)}</pre></article></div>}
      {section === "doctor" && <div className="admin-grid">{snapshot.doctor.map((item) => <article className="card" key={item.id}><h2>{item.id}</h2><p><span className="status-pill">{item.status}</span></p><p>{item.detail}</p></article>)}</div>}
      {(section === "formats" || section === "models" || section === "providers" || section === "plugins" || section === "configuration" || section === "doctor") && <aside className="card authorization-box">{section !== "configuration" && <label><input type="checkbox" checked={network} onChange={(event) => setNetwork(event.target.checked)} />{t("authorizeAdminNetwork")}</label>}{(section === "formats" || section === "models" || section === "providers" || section === "doctor") && <label><input type="checkbox" checked={privateNetwork} onChange={(event) => setPrivateNetwork(event.target.checked)} />{t("allowPrivateNetwork")}</label>}{section === "models" && <label><input type="checkbox" checked={insecure} onChange={(event) => setInsecure(event.target.checked)} />{t("allowInsecureTransport")}</label>}<label><input type="checkbox" checked={dangerous} onChange={(event) => setDangerous(event.target.checked)} />{t("authorizeDangerous")}</label><p>{t("oneShotAuthorization")}</p></aside>}
    </>}
  </section>;
}
