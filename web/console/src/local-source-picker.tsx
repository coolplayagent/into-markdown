import { useEffect, useRef, useState, type ClipboardEvent } from "react";
import { ApiError, type ApiClient } from "./api";
import type { UploadEntry } from "./task-runtime";
import { useI18n } from "./i18n";
import { diagnosticLabel } from "./task-ui";
export function useLocalPicker(api: ApiClient, setEntries: (update: (current: UploadEntry[]) => UploadEntry[]) => void, addFiles: (files: File[]) => void, showMessage: (message: string) => void) {
  const { t } = useI18n();
  const [available, setAvailable] = useState(false);
  const [selecting, setSelecting] = useState(false);
  const controllerRef = useRef<AbortController | null>(null);
  useEffect(() => {
    const controller = new AbortController();
    void api.localAvailable?.(controller.signal).then(setAvailable).catch(() => setAvailable(false));
    return () => { controller.abort(); controllerRef.current?.abort(); };
  }, [api]);
  const select = async (kind: "pick" | "paste") => {
    if (controllerRef.current) return;
    const controller = new AbortController(); controllerRef.current = controller;
    setSelecting(true); showMessage("");
    try {
      if (kind === "paste" && !available) {
        if (!navigator.clipboard?.read) throw new ApiError("localSourceUnavailable");
        const items = await navigator.clipboard.read();
        const files: File[] = [];
        for (const item of items) {
          const type = item.types.find(type => type.startsWith("image/"));
          if (type) { const blob = await item.getType(type); files.push(new File([blob], `clipboard-${Date.now()}-${files.length}.${type.split("/")[1]}`, { type })); }
        }
        if (!files.length) throw new ApiError("clipboardEmpty");
        if (!controller.signal.aborted) addFiles(files);
        return;
      }
      if (!api.selectLocal) throw new ApiError("localSourceUnavailable");
      const files = await api.selectLocal(kind, controller.signal);
      setEntries(current => [...current, ...files.map(source => ({ key: `local:${source.id}`, file: new File([], source.name), originalSize: source.size, localSelection: source }))]);
    } catch (error) {
      if (!controller.signal.aborted) showMessage(diagnosticLabel(error instanceof ApiError ? error.code : "localSourceUnavailable", t));
    } finally { if (controllerRef.current === controller) { controllerRef.current = null; setSelecting(false); } }
  };
  const onPaste = (event: ClipboardEvent<HTMLElement>) => {
    if ((event.target as HTMLElement).closest("input, textarea, [contenteditable=true]")) return;
    const files = Array.from(event.clipboardData.files);
    if (files.length) { event.preventDefault(); addFiles(files); }
    else if (available) { event.preventDefault(); void select("paste"); }
  };
  return { available, selecting, select, onPaste, cancel: () => controllerRef.current?.abort() };
}
export function LocalSourceControls({ picker }: { picker: ReturnType<typeof useLocalPicker> }) {
  const { t } = useI18n();
  return <div className="picker-actions local-source-actions">
    {picker.available && <button type="button" className="secondary" disabled={picker.selecting} onClick={() => void picker.select("pick")}>{t("chooseLocalFiles")}</button>}
    <button type="button" className="secondary" disabled={picker.selecting} onClick={() => void picker.select("paste")}>{t("pasteFiles")}</button>
    {picker.selecting && <button type="button" className="text-button" onClick={picker.cancel}>{t("cancel")}</button>}
    <small>{t(picker.available ? "localImportHint" : "browserImportHint")}</small>
  </div>;
}
