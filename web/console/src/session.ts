const SESSION_FRAGMENT = "#into-md-session=";
const SESSION_PATTERN = /^[A-Za-z0-9_-]{43}$/;
const SESSION_STORAGE_KEY = "into-md.session";
type SessionStorage = Pick<Storage, "getItem" | "setItem" | "removeItem">;

export function takeSession(location: Pick<Location, "hash" | "pathname" | "search">,
  history: Pick<History, "replaceState">, storage?: SessionStorage): string | null {
  const hash = location.hash;
  const value = hash.startsWith(SESSION_FRAGMENT) ? hash.slice(SESSION_FRAGMENT.length) : "";
  history.replaceState(null, "", `${location.pathname}${location.search}`);
  if (hash) {
    if (!SESSION_PATTERN.test(value)) {
      try { storage?.removeItem(SESSION_STORAGE_KEY); } catch { /* fail closed below */ }
      return null;
    }
    try { storage?.setItem(SESSION_STORAGE_KEY, value); } catch { /* the current load can still continue */ }
    return value;
  }
  try {
    const saved = storage?.getItem(SESSION_STORAGE_KEY) ?? null;
    if (saved && SESSION_PATTERN.test(saved)) return saved;
    storage?.removeItem(SESSION_STORAGE_KEY);
  } catch { /* unavailable storage means a fresh handoff is required */ }
  return null;
}
