import { useEffect, useRef, type ReactNode } from "react";

export default function Modal({ children, onClose, label, wide = false }: {
  children: ReactNode; onClose: () => void; label: string; wide?: boolean;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const closeRef = useRef(onClose);
  closeRef.current = onClose;
  useEffect(() => {
    const previous = document.activeElement as HTMLElement | null;
    ref.current?.focus();
    const keydown = (event: KeyboardEvent) => {
      if (event.key === "Escape") { event.preventDefault(); closeRef.current(); }
      if (event.key !== "Tab") return;
      const controls = Array.from(ref.current?.querySelectorAll<HTMLElement>(
        'button:not(:disabled), a[href], input:not(:disabled), select, textarea, summary, [tabindex="0"]'
      ) || []).filter(element => !element.closest("[hidden]"));
      const first = controls[0];
      const last = controls.at(-1);
      if (!first) { event.preventDefault(); return; }
      if (event.shiftKey && (document.activeElement === first || document.activeElement === ref.current)) {
        event.preventDefault(); last?.focus();
      } else if (!event.shiftKey && (document.activeElement === last || document.activeElement === ref.current)) {
        event.preventDefault(); first.focus();
      }
    };
    document.addEventListener("keydown", keydown);
    return () => { document.removeEventListener("keydown", keydown); previous?.focus(); };
  }, []);
  return <div className="modal-backdrop" onClick={onClose}>
    <div className={"modal" + (wide ? " wide" : "")} role="dialog" aria-modal="true" aria-label={label}
      tabIndex={-1} ref={ref} onClick={event => event.stopPropagation()}>{children}</div>
  </div>;
}
