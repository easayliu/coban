import { Loader2Icon } from "lucide-react";
import type React from "react";
import { useI18n } from "@/lib/i18n";
import { cn } from "@/lib/utils";

export function Spinner({
  className,
  ...props
}: React.ComponentProps<typeof Loader2Icon>): React.ReactElement {
  const { t } = useI18n();

  return (
    <Loader2Icon
      aria-label={t("加载中", "Loading")}
      className={cn("animate-spin", className)}
      role="status"
      {...props}
    />
  );
}
