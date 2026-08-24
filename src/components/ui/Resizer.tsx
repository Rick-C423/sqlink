import { useCallback, useRef } from "react";
import { useT } from "@/i18n";

interface ResizerProps {
  /** vertical = 竖条，拖动调左右宽度；horizontal = 横条，拖动调上下高度 */
  orientation: "vertical" | "horizontal";
  /** 拖动增量回调：正数 = 向右（vertical）或向下（horizontal）移动的像素 */
  onDrag: (delta: number) => void;
  onDoubleClick?: () => void;
}

/**
 * 可拖拽分隔条。拖动期间把 cursor/userSelect 挂到 body 上，
 * 避免指针移出分隔条后样式闪断。
 */
export function Resizer({ orientation, onDrag, onDoubleClick }: ResizerProps) {
  const t = useT();
  const lastPos = useRef(0);

  const onMouseDown = useCallback(
    (e: React.MouseEvent) => {
      e.preventDefault();
      lastPos.current = orientation === "vertical" ? e.clientX : e.clientY;
      const cursor = orientation === "vertical" ? "col-resize" : "row-resize";

      const onMove = (ev: MouseEvent) => {
        const pos = orientation === "vertical" ? ev.clientX : ev.clientY;
        const delta = pos - lastPos.current;
        lastPos.current = pos;
        onDrag(delta);
      };
      const onUp = () => {
        window.removeEventListener("mousemove", onMove);
        window.removeEventListener("mouseup", onUp);
        document.body.style.cursor = "";
        document.body.style.userSelect = "";
      };
      window.addEventListener("mousemove", onMove);
      window.addEventListener("mouseup", onUp);
      document.body.style.cursor = cursor;
      document.body.style.userSelect = "none";
    },
    [orientation, onDrag]
  );

  return (
    <div
      onMouseDown={onMouseDown}
      onDoubleClick={onDoubleClick}
      title={t("resizer.title")}
      className={`flex-shrink-0 transition-colors hover:bg-muted-foreground/40 ${
        orientation === "vertical" ? "w-1 cursor-col-resize" : "h-1 cursor-row-resize"
      }`}
    />
  );
}
