import { t } from "@/lib/i18n";
import { useLocale } from "@/hooks/useLocale";
import { useCallback, useEffect, useId, useLayoutEffect, useRef, useState, type CSSProperties, type KeyboardEvent, type ReactNode } from "react";
import Check from "lucide-react/dist/esm/icons/check.js";
import ChevronDown from "lucide-react/dist/esm/icons/chevron-down.js";

export interface SelectOption {
  value: string;
  label: string;
  /** Secondary text shown after the label in the list, e.g. latency. */
  hint?: string;
  disabled?: boolean;
}

interface SelectProps {
  value: string;
  options: SelectOption[];
  onChange: (value: string) => void;
  ariaLabel: string;
  placeholder?: string;
  disabled?: boolean;
  /** field: full-width form input; compact: toolbar-sized. */
  variant?: "field" | "compact";
  icon?: ReactNode;
  className?: string;
}

const MENU_MAX_HEIGHT = 280;

/**
 * Themed replacement for the native <select>, following the ARIA
 * select-only combobox pattern: the button keeps focus and
 * aria-activedescendant points at the highlighted option.
 */
export function Select({
  value,
  options,
  onChange,
  ariaLabel,
  placeholder = t("请选择"),
  disabled = false,
  variant = "field",
  icon,
  className,
}: SelectProps) {
  useLocale();
  const id = useId();
  const triggerRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLUListElement>(null);
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(-1);
  const [menuStyle, setMenuStyle] = useState<CSSProperties>({});
  const selected = options.find((option) => option.value === value);

  const enabledIndex = useCallback((from: number, step: 1 | -1) => {
    for (let index = from; index >= 0 && index < options.length; index += step) {
      if (!options[index].disabled) return index;
    }
    return -1;
  }, [options]);

  const openMenu = () => {
    if (disabled || !options.length) return;
    const current = options.findIndex((option) => option.value === value && !option.disabled);
    setActive(current >= 0 ? current : enabledIndex(0, 1));
    setOpen(true);
  };

  const close = useCallback(() => setOpen(false), []);

  const choose = (index: number) => {
    const option = options[index];
    if (!option || option.disabled) return;
    setOpen(false);
    if (option.value !== value) onChange(option.value);
  };

  // Fixed positioning keeps the menu visible inside scrolling or clipped
  // panels; it opens upwards when there is no room below.
  useLayoutEffect(() => {
    if (!open || !triggerRef.current) return;
    const rect = triggerRef.current.getBoundingClientRect();
    const below = window.innerHeight - rect.bottom - 8;
    const upwards = below < Math.min(MENU_MAX_HEIGHT, options.length * 32 + 8) && rect.top > below;
    setMenuStyle({
      left: rect.left,
      minWidth: rect.width,
      maxHeight: Math.max(120, Math.min(MENU_MAX_HEIGHT, (upwards ? rect.top : below) - 8)),
      ...(upwards ? { bottom: window.innerHeight - rect.top + 4 } : { top: rect.bottom + 4 }),
    });
  }, [open, options.length]);

  useEffect(() => {
    if (!open) return;
    const onPointer = (event: PointerEvent) => {
      const target = event.target as Node;
      if (!triggerRef.current?.contains(target) && !menuRef.current?.contains(target)) close();
    };
    const onViewportChange = (event: Event) => {
      if (event.target instanceof Node && menuRef.current?.contains(event.target)) return;
      close();
    };
    document.addEventListener("pointerdown", onPointer);
    window.addEventListener("resize", onViewportChange);
    window.addEventListener("scroll", onViewportChange, true);
    return () => {
      document.removeEventListener("pointerdown", onPointer);
      window.removeEventListener("resize", onViewportChange);
      window.removeEventListener("scroll", onViewportChange, true);
    };
  }, [open, close]);

  useEffect(() => {
    if (!open || active < 0) return;
    menuRef.current?.querySelector<HTMLElement>(`[data-index="${active}"]`)?.scrollIntoView({ block: "nearest" });
  }, [open, active]);

  const onKeyDown = (event: KeyboardEvent<HTMLButtonElement>) => {
    if (disabled) return;
    const key = event.key;
    if (!open) {
      if (["ArrowDown", "ArrowUp", "Enter", " "].includes(key)) {
        event.preventDefault();
        openMenu();
      }
      return;
    }
    if (key === "Escape" || key === "Tab") {
      if (key === "Escape") event.preventDefault();
      close();
      return;
    }
    if (key === "Enter" || key === " ") {
      event.preventDefault();
      choose(active);
      return;
    }
    const next =
      key === "ArrowDown" ? enabledIndex(active + 1, 1)
        : key === "ArrowUp" ? enabledIndex(active - 1, -1)
          : key === "Home" ? enabledIndex(0, 1)
            : key === "End" ? enabledIndex(options.length - 1, -1)
              : null;
    if (next === null) return;
    event.preventDefault();
    if (next >= 0) setActive(next);
  };

  return (
    <div className={["ui-select", `ui-select--${variant}`, className].filter(Boolean).join(" ")}>
      <button
        ref={triggerRef}
        type="button"
        role="combobox"
        className="ui-select__trigger"
        aria-label={ariaLabel}
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-controls={`${id}-listbox`}
        aria-activedescendant={open && active >= 0 ? `${id}-option-${active}` : undefined}
        disabled={disabled}
        onClick={() => (open ? close() : openMenu())}
        onKeyDown={onKeyDown}
      >
        {icon ? <span className="ui-select__icon" aria-hidden="true">{icon}</span> : null}
        <span className={selected ? "ui-select__value" : "ui-select__value ui-select__value--placeholder"}>
          {selected?.label ?? placeholder}
        </span>
        <ChevronDown size={14} className="ui-select__chevron" aria-hidden="true" />
      </button>
      {open ? (
        <ul ref={menuRef} id={`${id}-listbox`} role="listbox" aria-label={ariaLabel} className="ui-select__menu" style={menuStyle}>
          {options.map((option, index) => (
            <li
              key={option.value}
              id={`${id}-option-${index}`}
              data-index={index}
              role="option"
              aria-selected={option.value === value}
              aria-disabled={option.disabled || undefined}
              className={[
                "ui-select__option",
                index === active ? "ui-select__option--active" : "",
                option.disabled ? "ui-select__option--disabled" : "",
              ].filter(Boolean).join(" ")}
              onPointerEnter={() => !option.disabled && setActive(index)}
              onPointerDown={(event) => event.preventDefault()}
              onClick={() => choose(index)}
            >
              <span className="ui-select__check" aria-hidden="true">{option.value === value ? <Check size={13} /> : null}</span>
              <span className="ui-select__label">{option.label}</span>
              {option.hint ? <span className="ui-select__hint">{option.hint}</span> : null}
            </li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}
