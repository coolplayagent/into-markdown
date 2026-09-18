import { createContext, useContext, useEffect, useMemo, useSyncExternalStore, type ReactNode } from "react";
import type { ApiClient } from "./api";
import { TaskRuntime } from "./task-runtime";
const Context = createContext<TaskRuntime | null>(null);
export function TaskProvider({ api, children }: { api: ApiClient; children: ReactNode }) {
  const runtime = useMemo(() => new TaskRuntime(api), [api]);
  useEffect(() => { runtime.start(); return () => runtime.stop(); }, [runtime]);
  return <Context.Provider value={runtime}>{children}</Context.Provider>;
}
export function useTaskRuntime() { return useContext(Context); }
export function useTaskRevision() {
  const runtime = useTaskRuntime();
  return useSyncExternalStore(runtime?.subscribe ?? (() => () => {}), runtime?.snapshot ?? (() => 0));
}
export function useTaskApi(fallback: ApiClient) { return useTaskRuntime()?.api ?? fallback; }

export function useObservedTaskRuntime() {
  useTaskRevision();
  return useTaskRuntime();
}
