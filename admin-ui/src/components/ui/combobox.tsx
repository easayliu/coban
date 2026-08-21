"use client";

import { Combobox as ComboboxPrimitive } from "@base-ui/react/combobox";
import { CheckIcon, ChevronsUpDownIcon, SearchIcon } from "lucide-react";
import type * as React from "react";
import { cn } from "@/lib/utils";
import { useI18n } from "@/lib/i18n";
import {
  selectTriggerIconClassName,
  selectTriggerVariants,
} from "@/components/ui/select";

export const Combobox: typeof ComboboxPrimitive.Root = ComboboxPrimitive.Root;

export function ComboboxTrigger({
  className,
  children,
  ...props
}: ComboboxPrimitive.Trigger.Props): React.ReactElement {
  return (
    <ComboboxPrimitive.Trigger
      className={cn(selectTriggerVariants(), "min-w-0", className)}
      data-slot="combobox-trigger"
      {...props}
    >
      {children}
      <ComboboxPrimitive.Icon data-slot="combobox-icon">
        <ChevronsUpDownIcon className={selectTriggerIconClassName} />
      </ComboboxPrimitive.Icon>
    </ComboboxPrimitive.Trigger>
  );
}

export function ComboboxValue({
  className,
  ...props
}: ComboboxPrimitive.Value.Props & { className?: string }): React.ReactElement {
  return (
    <span
      className={cn(
        "flex-1 truncate in-data-placeholder:text-muted-foreground",
        className,
      )}
      data-slot="combobox-value"
    >
      <ComboboxPrimitive.Value {...props} />
    </span>
  );
}

export function ComboboxPopup({
  className,
  children,
  inputPlaceholder,
  emptyText,
  side = "bottom",
  sideOffset = 4,
  align = "start",
  ...props
}: Omit<ComboboxPrimitive.Popup.Props, "children"> & {
  children: ComboboxPrimitive.List.Props["children"];
  inputPlaceholder?: string;
  emptyText?: React.ReactNode;
  side?: ComboboxPrimitive.Positioner.Props["side"];
  sideOffset?: ComboboxPrimitive.Positioner.Props["sideOffset"];
  align?: ComboboxPrimitive.Positioner.Props["align"];
}): React.ReactElement {
  const { t } = useI18n();
  const resolvedInputPlaceholder =
    inputPlaceholder === undefined ? t("搜索…", "Search…") : inputPlaceholder;
  const resolvedEmptyText =
    emptyText === undefined ? t("没有匹配项", "No matching options") : emptyText;

  return (
    <ComboboxPrimitive.Portal>
      <ComboboxPrimitive.Positioner
        align={align}
        className="z-50 select-none outline-none"
        side={side}
        sideOffset={sideOffset}
      >
        <ComboboxPrimitive.Popup
          className={cn(
            "relative flex max-h-(--available-height) min-w-(--anchor-width) max-w-(--available-width) origin-(--transform-origin) flex-col overflow-hidden rounded-lg border bg-popover not-dark:bg-clip-padding text-foreground shadow-lg/5 outline-none transition-[scale,opacity] duration-100 data-ending-style:scale-98 data-ending-style:opacity-0 data-starting-style:scale-98 data-starting-style:opacity-0 before:pointer-events-none before:absolute before:inset-0 before:rounded-[calc(var(--radius-lg)-1px)] before:shadow-[0_1px_--theme(--color-black/4%)] dark:before:shadow-[0_-1px_--theme(--color-white/6%)]",
            className,
          )}
          data-slot="combobox-popup"
          {...props}
        >
          <div className="relative z-10 flex min-h-9 items-center gap-2 border-b px-2.5 sm:min-h-8">
            <SearchIcon className="size-4 shrink-0 text-muted-foreground" aria-hidden="true" />
            <ComboboxPrimitive.Input
              aria-label={t("搜索选项", "Search options")}
              autoComplete="off"
              className="min-w-0 flex-1 bg-transparent text-base outline-none placeholder:text-muted-foreground/72 sm:text-sm"
              placeholder={resolvedInputPlaceholder}
            />
          </div>
          <ComboboxPrimitive.Empty className="relative z-10">
            <div className="px-3 py-4 text-center text-muted-foreground text-sm">
              {resolvedEmptyText}
            </div>
          </ComboboxPrimitive.Empty>
          <ComboboxPrimitive.List
            className="relative z-10 min-h-0 overflow-y-auto overscroll-contain p-1 data-empty:p-0"
            data-slot="combobox-list"
          >
            {children}
          </ComboboxPrimitive.List>
        </ComboboxPrimitive.Popup>
      </ComboboxPrimitive.Positioner>
    </ComboboxPrimitive.Portal>
  );
}

export function ComboboxItem({
  className,
  children,
  ...props
}: ComboboxPrimitive.Item.Props): React.ReactElement {
  return (
    <ComboboxPrimitive.Item
      className={cn(
        "grid min-h-8 cursor-default grid-cols-[1rem_1fr] items-center gap-2 rounded-sm py-1 ps-2 pe-4 text-sm outline-none data-disabled:pointer-events-none data-highlighted:bg-accent data-highlighted:text-accent-foreground data-disabled:opacity-64 sm:min-h-7 [&_svg]:pointer-events-none [&_svg]:size-4 [&_svg]:shrink-0",
        className,
      )}
      data-slot="combobox-item"
      {...props}
    >
      <ComboboxPrimitive.ItemIndicator className="col-start-1">
        <CheckIcon aria-hidden="true" />
      </ComboboxPrimitive.ItemIndicator>
      <span className="col-start-2 min-w-0 truncate">{children}</span>
    </ComboboxPrimitive.Item>
  );
}
