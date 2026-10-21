import { useEffect, useRef } from "react";
import { useGridDraftStore } from "@/stores/gridDraftStore";

export function DraftNavigationDialog() {
  const navigation = useGridDraftStore((state) => state.navigation);
  const choose = useGridDraftStore((state) => state.chooseNavigation);
  const cancelRef = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    if (!navigation) return;
    const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    cancelRef.current?.focus();
    return () => previous?.focus();
  }, [!!navigation]);
  if (!navigation) return null;
  return (
    <div className="fixed inset-0 z-[100] flex items-center justify-center bg-black/40" onMouseDown={(event) => {
      if (event.target === event.currentTarget && !navigation.saving) void choose("cancel");
    }}>
      <div role="dialog" aria-modal="true" aria-labelledby="grid-draft-title" aria-describedby="grid-draft-description"
        className="w-[420px] max-w-[calc(100vw-32px)] rounded-lg border bg-background p-5 shadow-xl"
        onKeyDown={(event) => {
          if (event.key === "Escape") { event.preventDefault(); void choose("cancel"); }
          if (event.key === "Tab") {
            const buttons = Array.from(event.currentTarget.querySelectorAll<HTMLButtonElement>("button:not(:disabled)"));
            const first = buttons[0]; const last = buttons[buttons.length - 1];
            if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last?.focus(); }
            if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first?.focus(); }
          }
        }}>
        <h2 id="grid-draft-title" className="text-base font-semibold">有未保存的表格改动</h2>
        <p id="grid-draft-description" className="mt-2 text-sm text-muted-foreground">继续操作前，请选择保存或放弃草稿。取消会保留当前页面和全部改动。</p>
        <div className="mt-5 flex justify-end gap-2 text-sm">
          <button ref={cancelRef} className="rounded border px-3 py-1.5 disabled:opacity-50" disabled={navigation.saving} onClick={() => choose("cancel")}>取消</button>
          <button className="rounded border px-3 py-1.5 disabled:opacity-50" disabled={navigation.saving} onClick={() => choose("discard")}>放弃改动</button>
          <button className="rounded bg-primary px-3 py-1.5 text-primary-foreground disabled:opacity-50" disabled={navigation.saving} onClick={() => choose("save")}>{navigation.saving ? "正在保存…" : "保存并继续"}</button>
        </div>
      </div>
    </div>
  );
}
