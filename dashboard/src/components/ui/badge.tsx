import * as React from "react";
import { cva, type VariantProps } from "class-variance-authority";

import { cn } from "@/lib/utils";

// Outline status badge, the same shape as Mission Control's: a thin tinted
// border and coloured text on a transparent fill. Callers showing a code (an
// id, a band) add `uppercase` themselves; the badge never shouts by default.
const badgeVariants = cva(
  "inline-flex items-center gap-1 whitespace-nowrap rounded-[4px] border bg-transparent font-medium leading-none",
  {
    variants: {
      variant: {
        success: "border-ok/40 text-ok",
        warning: "border-warn/40 text-warn",
        serious: "border-serious/40 text-serious",
        error: "border-err/40 text-err",
        info: "border-info/40 text-info",
        neutral: "border-border text-muted-foreground",
      },
      size: {
        sm: "h-5 px-1.5 text-[11px]",
        md: "h-6 px-2 text-xs",
      },
    },
    defaultVariants: { variant: "neutral", size: "sm" },
  },
);

export type BadgeVariant = NonNullable<VariantProps<typeof badgeVariants>["variant"]>;

export interface BadgeProps
  extends React.HTMLAttributes<HTMLDivElement>,
    VariantProps<typeof badgeVariants> {}

export function Badge({ className, variant, size, ...props }: BadgeProps) {
  return <div className={cn(badgeVariants({ variant, size }), className)} {...props} />;
}
