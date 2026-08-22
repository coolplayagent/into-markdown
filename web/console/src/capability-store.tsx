import { createContext, type ReactNode, useCallback, useContext, useEffect, useMemo, useRef, useState } from "react";
import type { ApiClient, CapabilityQuickView, CapabilitySnapshot } from "./api";

interface CapabilityContextValue {
  snapshot: CapabilitySnapshot | null;
  error: boolean;
  refresh: () => Promise<void>;
  capability: (id: CapabilityQuickView["id"]) => CapabilityQuickView | undefined;
}

const CapabilityContext = createContext<CapabilityContextValue | null>(null);

export function CapabilityProvider({ api, children }: { api: ApiClient; children: ReactNode }) {
  const [snapshot, setSnapshot] = useState<CapabilitySnapshot | null>(null);
  const [error, setError] = useState(false);
  const active = useRef<AbortController | null>(null);
  const refresh = useCallback(async () => {
    active.current?.abort();
    const controller = new AbortController(); active.current = controller;
    try {
      const fast = (api as Partial<ApiClient>).capabilitySnapshot;
      const next = fast ? await fast.call(api, controller.signal) : await legacySnapshot(api, controller.signal);
      setSnapshot((current) => !current || next.generation >= current.generation ? next : current);
      setError(false);
    } catch (reason) {
      if (!(reason instanceof DOMException && reason.name === "AbortError")) setError(true);
    }
  }, [api]);
  useEffect(() => {
    void refresh();
    const timer = window.setInterval(() => void refresh(), 10_000);
    const changed = () => void refresh();
    window.addEventListener("into-md-capabilities-changed", changed);
    return () => { window.clearInterval(timer); window.removeEventListener("into-md-capabilities-changed", changed); active.current?.abort(); };
  }, [refresh]);
  const value = useMemo<CapabilityContextValue>(() => ({ snapshot, error, refresh,
    capability: (id) => snapshot?.capabilities.find((item) => item.id === id) }), [error, refresh, snapshot]);
  return <CapabilityContext.Provider value={value}>{children}</CapabilityContext.Provider>;
}

async function legacySnapshot(api: ApiClient, signal: AbortSignal): Promise<CapabilitySnapshot> {
  const [status, admin] = await Promise.all([api.status(signal), api.admin(signal)]);
  return { schemaVersion: 2, generation: 0, checking: false, capabilities: admin.capabilities.map((item) => ({
    ...item, name: item.id, currentSourceName: item.currentSource,
    ...(item.id === "ocr" ? { status: status.imageOcr.available ? "ready" as const : item.status } : {}),
    ...(item.id === "transcription" && status.audioTranscription ? { status: status.audioTranscription.available ? "ready" as const : item.status } : {}),
    ...(item.id === "diarization" && status.speakerDiarization ? { status: status.speakerDiarization.available ? "ready" as const : item.status } : {}),
  })) };
}

export function useCapabilities() {
  const value = useContext(CapabilityContext);
  if (!value) throw new Error("CapabilityProvider is missing");
  return value;
}
