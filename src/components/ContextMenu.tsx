import { useEffect, useRef, useState } from "react";

export interface MenuItem {
  label: string;
  icon?: React.ReactNode;
  danger?: boolean;
  onClick: () => void;
}

interface ContextMenuProps {
  x: number;
  y: number;
  items: MenuItem[];
  onClose: () => void;
}

/** 轻量右键菜单：点击外部 / Esc 关闭 */
export function ContextMenu({ x, y, items, onClose }: ContextMenuProps) {
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const handleClick = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) onClose();
    };
    const handleKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("mousedown", handleClick);
    window.addEventListener("keydown", handleKey);
    return () => {
      window.removeEventListener("mousedown", handleClick);
      window.removeEventListener("keydown", handleKey);
    };
  }, [onClose]);

  // 防止菜单超出窗口右/下边缘
  const [pos, setPos] = useState({ x, y });
  useEffect(() => {
    if (!ref.current) return;
    const rect = ref.current.getBoundingClientRect();
    setPos({
      x: Math.min(x, window.innerWidth - rect.width - 8),
      y: Math.min(y, window.innerHeight - rect.height - 8),
    });
  }, [x, y]);

  return (
    <div
      ref={ref}
      className="fixed z-50 min-w-[160px] rounded-md border bg-white dark:bg-gray-900 shadow-lg py-1"
      style={{ left: pos.x, top: pos.y }}
    >
      {items.map((item, i) => (
        <button
          key={i}
          className={`w-full flex items-center gap-2 px-3 py-1.5 text-sm text-left hover:bg-gray-100 dark:hover:bg-gray-800 ${
            item.danger ? "text-red-600" : ""
          }`}
          onClick={() => {
            onClose();
            item.onClick();
          }}
        >
          {item.icon}
          {item.label}
        </button>
      ))}
    </div>
  );
}
