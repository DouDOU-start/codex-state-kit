import { useEffect, useMemo, useRef, useState } from "react";
import X from "lucide-react/dist/esm/icons/x.js";
import type { ProxyGroup } from "@/types";
import { LatencyProbe } from "@/components/LatencyProbe";

export function MihomoGroupPanel({
  group,
  probing,
  onSelect,
  onProbe,
  onProbeAll,
}: {
  group: ProxyGroup;
  probing: boolean;
  onSelect: (node: string) => void;
  onProbe: () => void;
  onProbeAll: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const dialogRef = useRef<HTMLDialogElement>(null);
  const selectable = group.groupType === "select";
  const current = group.all.find((node) => node.name === group.now) ?? null;
  const nodes = useMemo(() => {
    const needle = query.trim().toLowerCase();
    if (!needle) return group.all;
    return group.all.filter((node) =>
      node.name.toLowerCase().includes(needle) || node.nodeType.toLowerCase().includes(needle),
    );
  }, [group.all, query]);

  useEffect(() => {
    const dialog = dialogRef.current;
    if (!open || !dialog) return;
    if (!dialog.open) dialog.showModal();
    return () => {
      if (dialog.open) dialog.close();
    };
  }, [open]);

  return (
    <div className="mihomo-picker">
      <div className="mihomo-picker__current">
        <span>当前节点</span>
        <strong>{group.now || "未选择"}</strong>
        <small>
          {current?.nodeType ?? group.groupType}
          {current?.delay != null ? ` · ${current.delay} ms` : ""}
          {` · ${group.all.length} 个`}
        </small>
      </div>
      <div className="mihomo-picker__actions">
        <button
          type="button"
          className="button button--secondary"
          disabled={!selectable}
          onClick={() => {
            setQuery("");
            setOpen(true);
          }}
        >
          选择节点
        </button>
        <LatencyProbe
          probing={probing}
          disabled={!group.now}
          sample={current?.delay != null ? { name: current.name, delayMs: current.delay, error: null } : null}
          onProbe={onProbe}
        />
      </div>
      {open ? (
        <dialog
          ref={dialogRef}
          className="node-picker"
          aria-label="选择节点"
          onCancel={(event) => {
            event.preventDefault();
            setOpen(false);
          }}
          onClick={(event) => {
            if (event.target === dialogRef.current) setOpen(false);
          }}
        >
          <header className="node-picker__header">
            <div>
              <h2>选择节点</h2>
              <p>{group.name} · {group.all.length} 个</p>
              <button type="button" className="button button--ghost" disabled={probing} onClick={onProbeAll}>{probing ? "正在测速…" : "测全部节点"}</button>
            </div>
            <button type="button" className="network-log-close" aria-label="关闭" onClick={() => setOpen(false)}>
              <X size={16} />
            </button>
          </header>
          <label className="node-picker__search">
            <span>筛选</span>
            <input
              type="text"
              spellCheck={false}
              autoComplete="off"
              placeholder="节点名称"
              value={query}
              onChange={(event) => setQuery(event.target.value)}
            />
          </label>
          <div className="node-picker__list">
            {nodes.length === 0 ? <p className="node-picker__empty">没有匹配的节点</p> : nodes.map((node) => {
              const active = node.name === group.now;
              return (
                <button
                  key={node.name}
                  type="button"
                  className={`node-picker__item${active ? " node-picker__item--active" : ""}`}
                  onClick={() => {
                    onSelect(node.name);
                    setOpen(false);
                  }}
                >
                  <span>{node.name}</span>
                  <small>
                    {node.nodeType}
                    {node.delay != null ? ` · ${node.delay} ms` : ""}
                    {active ? " · 当前" : ""}
                  </small>
                </button>
              );
            })}
          </div>
        </dialog>
      ) : null}
    </div>
  );
}
