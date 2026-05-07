import { ChevronLeft, ChevronRight } from "lucide-react";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

export interface WizardStep {
  id: string;
  label: string;
  description?: string;
}

interface WizardShellProps {
  steps: ReadonlyArray<WizardStep>;
  currentStepId: string;
  onChangeStep: (id: string) => void;
  children: React.ReactNode;
  onBack?: () => void;
  onNext?: () => void;
  nextLabel?: string;
  nextDisabled?: boolean;
  nextLoading?: boolean;
  backLabel?: string;
  backDisabled?: boolean;
  rightAction?: React.ReactNode;
}

export function WizardShell({
  steps,
  currentStepId,
  onChangeStep,
  children,
  onBack,
  onNext,
  nextLabel = "Next",
  nextDisabled,
  nextLoading,
  backLabel = "Back",
  backDisabled,
  rightAction,
}: WizardShellProps) {
  const currentIdx = steps.findIndex((s) => s.id === currentStepId);
  const current = steps[currentIdx] ?? steps[0];

  return (
    <div className="space-y-6 max-w-3xl">
      <ol className="flex items-center gap-1">
        {steps.map((step, idx) => {
          const isActive = step.id === currentStepId;
          const isPast = idx < currentIdx;
          return (
            <li key={step.id} className="flex-1 flex items-center gap-2">
              <button
                type="button"
                onClick={() => onChangeStep(step.id)}
                className="flex items-center gap-2 group"
              >
                <span
                  className={cn(
                    "h-6 w-6 rounded-full flex items-center justify-center text-[11px] font-medium transition-colors",
                    isActive
                      ? "bg-primary text-primary-foreground"
                      : isPast
                        ? "bg-ok/20 text-ok"
                        : "bg-muted text-muted-foreground",
                  )}
                >
                  {idx + 1}
                </span>
                <span
                  className={cn(
                    "text-xs font-medium uppercase tracking-wider transition-colors",
                    isActive
                      ? "text-foreground"
                      : isPast
                        ? "text-ok/80"
                        : "text-muted-foreground",
                  )}
                >
                  {step.label}
                </span>
              </button>
              {idx < steps.length - 1 && (
                <span
                  className={cn(
                    "flex-1 h-px",
                    idx < currentIdx ? "bg-ok/40" : "bg-border",
                  )}
                />
              )}
            </li>
          );
        })}
      </ol>

      <div className="space-y-1">
        <h1 className="text-xl font-semibold tracking-tight">{current?.label}</h1>
        {current?.description && (
          <p className="text-sm text-muted-foreground">{current.description}</p>
        )}
      </div>

      <div className="space-y-4">{children}</div>

      <div className="flex items-center justify-between pt-4 border-t border-border">
        <Button
          variant="ghost"
          onClick={onBack}
          disabled={backDisabled || !onBack}
        >
          <ChevronLeft />
          {backLabel}
        </Button>
        <div className="flex items-center gap-3">
          {rightAction}
          <Button
            onClick={onNext}
            disabled={nextDisabled || !onNext}
            aria-busy={nextLoading || undefined}
          >
            {nextLoading ? "Working…" : nextLabel}
            <ChevronRight />
          </Button>
        </div>
      </div>
    </div>
  );
}
