const SESSION_FRAGMENT = "#into-md-session=";
const SESSION_PATTERN = /^[A-Za-z0-9_-]{43}$/;

export function takeSession(location: Pick<Location, "hash" | "pathname" | "search">, history: Pick<History, "replaceState">): string | null {
  const hash = location.hash;
  const value = hash.startsWith(SESSION_FRAGMENT) ? hash.slice(SESSION_FRAGMENT.length) : "";
  const session = SESSION_PATTERN.test(value) ? value : null;
  history.replaceState(null, "", `${location.pathname}${location.search}`);
  return session;
}
