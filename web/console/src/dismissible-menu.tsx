import { useEffect, useRef, useState } from "react";
import type { ReactNode } from "react";

export function DismissibleMenu({ label, trigger, children, className = "task-menu" }: {
  label: string;
  trigger: ReactNode;
  children: ReactNode;
  className?: string;
}) {
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  const triggerButton = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (!open) return;
    const dismissOutside = (event: PointerEvent) => {
      if (event.target && !root.current?.contains(event.target as Node)) setOpen(false);
    };
    const dismissWithKeyboard = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      setOpen(false);
      triggerButton.current?.focus();
    };
    document.addEventListener("pointerdown", dismissOutside);
    document.addEventListener("keydown", dismissWithKeyboard);
    return () => {
      document.removeEventListener("pointerdown", dismissOutside);
      document.removeEventListener("keydown", dismissWithKeyboard);
    };
  }, [open]);

  return <div className={className} ref={root}>
    <button ref={triggerButton} className="menu-trigger" type="button" aria-label={label} aria-expanded={open} aria-haspopup="menu" onClick={() => setOpen((value) => !value)}>{trigger}</button>
    {open && <div className="task-menu-popover" role="menu" onClick={(event) => {
      if ((event.target as Element).closest("button,a,[role=menuitem]")) setOpen(false);
    }}>{children}</div>}
  </div>;
}
