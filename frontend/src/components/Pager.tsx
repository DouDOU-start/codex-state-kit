import ChevronLeft from "lucide-react/dist/esm/icons/chevron-left.js";
import ChevronRight from "lucide-react/dist/esm/icons/chevron-right.js";
import { Select } from "@/components/Select";

export const PAGE_SIZES = [20, 50, 100, 200];

interface PagerProps {
  /** Zero-based. */
  page: number;
  pageSize: number;
  total: number;
  disabled?: boolean;
  onPage: (page: number) => void;
  onPageSize: (pageSize: number) => void;
}

/** Page numbers around the current page, with gaps as null. */
function pageItems(page: number, pageCount: number): (number | null)[] {
  if (pageCount <= 7) return Array.from({ length: pageCount }, (_, index) => index);
  const pages = new Set([0, pageCount - 1, page - 1, page, page + 1]);
  if (page <= 3) [1, 2, 3, 4].forEach((index) => pages.add(index));
  if (page >= pageCount - 4) [pageCount - 5, pageCount - 4, pageCount - 3, pageCount - 2].forEach((index) => pages.add(index));
  const sorted = [...pages].filter((index) => index >= 0 && index < pageCount).sort((a, b) => a - b);
  const items: (number | null)[] = [];
  sorted.forEach((index, position) => {
    if (position > 0 && index - sorted[position - 1] > 1) items.push(null);
    items.push(index);
  });
  return items;
}

export function Pager({ page, pageSize, total, disabled, onPage, onPageSize }: PagerProps) {
  const pageCount = Math.max(1, Math.ceil(total / pageSize));
  return (
    <nav className="pager" aria-label="分页">
      <div className="pager__summary">
        <span>共 {total} 条</span>
        <Select
          variant="compact"
          ariaLabel="每页条数"
          value={String(pageSize)}
          options={PAGE_SIZES.map((size) => ({ value: String(size), label: `${size} 条/页` }))}
          onChange={(value) => onPageSize(Number(value))}
        />
      </div>
      <div className="pager__pages">
        <button type="button" className="pager__step" aria-label="上一页" disabled={disabled || page === 0} onClick={() => onPage(page - 1)}>
          <ChevronLeft size={14} />
        </button>
        {pageItems(page, pageCount).map((item, index) => item === null ? (
          <span key={`gap-${index}`} className="pager__gap">…</span>
        ) : (
          <button
            key={item}
            type="button"
            className="pager__page"
            aria-current={item === page ? "page" : undefined}
            disabled={disabled}
            onClick={() => onPage(item)}
          >
            {item + 1}
          </button>
        ))}
        <button type="button" className="pager__step" aria-label="下一页" disabled={disabled || page + 1 >= pageCount} onClick={() => onPage(page + 1)}>
          <ChevronRight size={14} />
        </button>
      </div>
    </nav>
  );
}
