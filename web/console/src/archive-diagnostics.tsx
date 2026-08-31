import { useEffect, useState } from "react";
import type { ApiClient, TaskRecord } from "./api";
import { useI18n } from "./i18n";

export function archiveMembers(text: string): string[] {
  const value: unknown = JSON.parse(text);
  if (!value || typeof value !== "object" || !("diagnostics" in value) || !Array.isArray(value.diagnostics)) return [];
  return value.diagnostics.slice(0, 1024).flatMap((item: unknown) => {
    if (!item || typeof item !== "object" || !("code" in item) || item.code !== "zip.entry.archiveExtractionRequired" || !("locator" in item)) return [];
    const locator = item.locator;
    return locator && typeof locator === "object" && "part" in locator && typeof locator.part === "string" && locator.part.length <= 4096 ? [locator.part] : [];
  });
}

export function ArchiveDiagnostics({ api, task }: { api: ApiClient; task: TaskRecord }) {
  const { t } = useI18n();
  const [members, setMembers] = useState<string[]>([]);
  const [unavailable, setUnavailable] = useState(false);
  const artifact = task.artifacts.find((item) => item.kind === "diagnostics");
  useEffect(() => {
    const controller = new AbortController();
    setMembers([]); setUnavailable(false);
    if (artifact) void api.preview(task.id, artifact.storageKey, controller.signal).then((preview) => {
      if (controller.signal.aborted) return;
      if (preview.truncated) { setUnavailable(true); return; }
      setMembers(archiveMembers(preview.text));
    }).catch(() => { if (!controller.signal.aborted) setUnavailable(true); });
    return () => controller.abort();
  }, [api, task.id, artifact?.storageKey]);
  if (unavailable) return <p role="status">{t("diagnosticsPreviewUnavailable")}</p>;
  return members.length ? <aside className="preview-notice" aria-label={t("diagnostics")}><ul>{members.map((member, index) => <li key={index}><strong>{member}</strong>: {t("archiveExtractionRequired")}</li>)}</ul></aside> : null;
}
