import type { ProxyGroup } from "@/types";

export function MihomoGroupPanel({
  group,
  probing,
  onSelect,
  onProbe,
}: {
  group: ProxyGroup;
  probing: boolean;
  onSelect: (node: string) => void;
  onProbe: () => void;
}) {
  const selectable = group.groupType === "select";
  return (
    <section className="mihomo-group">
      <header className="mihomo-group__header">
        <strong>{group.name}</strong>
        <span className="mihomo-group__type">{group.groupType}</span>
        <span className="mihomo-group__count">{group.all.length}</span>
        <button type="button" className="token-fetch-toggle" disabled={probing} onClick={onProbe}>
          {probing ? "测试中" : "测延迟"}
        </button>
      </header>
      <div className="mihomo-group__nodes">
        {group.all.map((node) => {
          const active = node.name === group.now;
          return (
            <button
              key={node.name}
              type="button"
              className={`mihomo-node${active ? " mihomo-node--active" : ""}`}
              disabled={!selectable}
              onClick={() => {
                if (selectable) onSelect(node.name);
              }}
            >
              <span className="mihomo-node__name">{node.name}</span>
              <span className="mihomo-node__meta">
                {node.nodeType}
                {node.delay != null ? ` · ${node.delay} ms` : ""}
              </span>
            </button>
          );
        })}
      </div>
    </section>
  );
}
