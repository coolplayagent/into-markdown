import { useEffect, useRef } from "react";

const FOCUSABLE = "a[href], button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), [contenteditable='true'], [tabindex]:not([tabindex='-1'])";

export function useDialogLifecycle<T extends HTMLElement>(
  open: boolean,
  close: () => void,
  canCloseOnEscape: (dialog: T) => boolean = () => true,
) {
  const dialogRef = useRef<T>(null);
  const closeRef = useRef(close);
  const canCloseRef = useRef(canCloseOnEscape);
  closeRef.current = close;
  canCloseRef.current = canCloseOnEscape;
  useEffect(() => {
    if (!open) return;
    const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const frame = window.requestAnimationFrame(() => dialogRef.current?.querySelector<HTMLElement>(FOCUSABLE)?.focus());
    const onKeyDown = (event: KeyboardEvent) => {
      const dialog = dialogRef.current;
      if (!dialog) return;
      const dialogs = document.querySelectorAll('[role="dialog"]');
      if (dialogs[dialogs.length - 1] !== dialog) return;
      if (event.key === "Tab") {
        const focusable = [...dialog.querySelectorAll<HTMLElement>(FOCUSABLE)];
        if (focusable.length === 0) { event.preventDefault(); dialog.focus(); return; }
        const first = focusable[0]!; const last = focusable[focusable.length - 1]!;
        const active = document.activeElement;
        if (event.shiftKey && (active === first || !dialog.contains(active))) { event.preventDefault(); last.focus(); }
        else if (!event.shiftKey && (active === last || !dialog.contains(active))) { event.preventDefault(); first.focus(); }
        return;
      }
      if (event.key !== "Escape" || !canCloseRef.current(dialog)) return;
      event.preventDefault();
      event.stopImmediatePropagation();
      closeRef.current();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => {
      window.cancelAnimationFrame(frame);
      window.removeEventListener("keydown", onKeyDown);
      if (previous?.isConnected) window.requestAnimationFrame(() => previous.focus());
    };
  }, [open]);
  return dialogRef;
}
