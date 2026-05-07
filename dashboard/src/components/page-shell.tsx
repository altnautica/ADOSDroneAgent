import type { ReactNode } from "react";

interface Props {
  title: string;
  blurb?: string;
  rightAction?: ReactNode;
  children: ReactNode;
  maxWidth?: string;
}

export function PageShell({
  title,
  blurb,
  rightAction,
  children,
  maxWidth = "max-w-5xl",
}: Props) {
  return (
    <div className={`space-y-6 ${maxWidth}`}>
      <header className="flex items-start justify-between gap-4">
        <div>
          <h1 className="text-xl font-semibold tracking-tight">{title}</h1>
          {blurb && (
            <p className="text-sm text-muted-foreground mt-1">{blurb}</p>
          )}
        </div>
        {rightAction && <div className="shrink-0">{rightAction}</div>}
      </header>
      {children}
    </div>
  );
}
