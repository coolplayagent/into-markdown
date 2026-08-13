import { createContext, type MouseEvent, type ReactNode, useContext, useEffect, useMemo, useState } from "react";

interface RouterValue {
  path: string;
  navigate(path: string): void;
}

const RouterContext = createContext<RouterValue | null>(null);

export function Router({ children }: { children: ReactNode }) {
  const [path, setPath] = useState(window.location.pathname);
  useEffect(() => {
    const update = () => setPath(window.location.pathname);
    window.addEventListener("popstate", update);
    return () => window.removeEventListener("popstate", update);
  }, []);
  const value = useMemo(
    () => ({
      path,
      navigate(next: string) {
        if (next !== window.location.pathname) window.history.pushState(null, "", next);
        setPath(next);
      },
    }),
    [path],
  );
  return <RouterContext.Provider value={value}>{children}</RouterContext.Provider>;
}

export function useRouter(): RouterValue {
  const value = useContext(RouterContext);
  if (!value) throw new Error("router is unavailable");
  return value;
}

export function RouteLink({ href, children, className }: { href: string; children: ReactNode; className?: string }) {
  const { navigate } = useRouter();
  const activate = (event: MouseEvent<HTMLAnchorElement>) => {
    if (event.button === 0 && !event.metaKey && !event.ctrlKey && !event.shiftKey && !event.altKey) {
      event.preventDefault();
      navigate(href);
    }
  };
  return <a href={href} className={className} onClick={activate}>{children}</a>;
}
