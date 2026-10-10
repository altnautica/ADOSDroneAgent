// A render-fault boundary so a fault shows a readable message instead of a
// white screen. It clears when `resetKey` changes (the caller passes the
// active route or screen), so a fault on one view costs that view, not the
// whole app; a genuinely permanent fault simply comes straight back.

import { Component, type ErrorInfo, type ReactNode } from "react";

interface Props {
  children: ReactNode;
  resetKey?: string;
  /** Custom fallback; the default is a plain card with Reload / Try again. */
  fallback?: (error: Error, reset: () => void) => ReactNode;
}

interface State {
  error: Error | null;
  caughtAt: string | undefined;
}

export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null, caughtAt: undefined };

  static getDerivedStateFromError(error: Error): Partial<State> {
    return { error };
  }

  static getDerivedStateFromProps(props: Props, state: State): Partial<State> | null {
    if (state.caughtAt === props.resetKey) return null;
    return state.error === null
      ? { caughtAt: props.resetKey }
      : { error: null, caughtAt: props.resetKey };
  }

  componentDidCatch(error: Error, info: ErrorInfo): void {
    console.error("render error", error, info.componentStack);
  }

  reset = () => {
    this.setState({ error: null });
  };

  render(): ReactNode {
    const { error } = this.state;
    if (!error) return this.props.children;
    if (this.props.fallback) return this.props.fallback(error, this.reset);
    return (
      <div
        role="alert"
        className="flex h-full w-full flex-col items-center justify-center gap-3 bg-background p-6 text-center"
      >
        <p className="text-lg font-semibold text-err">Something went wrong</p>
        <p className="max-w-xl select-text font-mono text-sm text-muted-foreground">
          {error.message}
        </p>
        <div className="flex gap-2">
          <button
            type="button"
            onClick={this.reset}
            className="min-h-12 rounded-md border border-border px-4 text-sm text-foreground hover:bg-muted"
          >
            Try again
          </button>
          <button
            type="button"
            onClick={() => window.location.reload()}
            className="min-h-12 rounded-md bg-primary px-4 text-sm text-primary-foreground hover:bg-primary-hover"
          >
            Reload
          </button>
        </div>
      </div>
    );
  }
}
