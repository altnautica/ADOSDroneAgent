import { AlertTriangle, RefreshCcw } from "lucide-react";
import { Component, type ErrorInfo, type ReactNode } from "react";

interface Props {
  children: ReactNode;
}

interface State {
  error: Error | null;
}

export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo): void {
    // Surface in console for debugging — agent log buffer doesn't see this.
    console.error("[dashboard] error boundary caught", error, info);
  }

  reset = () => {
    this.setState({ error: null });
  };

  render() {
    const { error } = this.state;
    if (!error) return this.props.children;

    return (
      <div className="container mx-auto px-4 py-10 max-w-2xl">
        <div className="rounded-lg border border-destructive/40 bg-destructive/5 p-6 space-y-4">
          <div className="flex items-start gap-3">
            <AlertTriangle className="h-5 w-5 text-destructive mt-0.5 shrink-0" />
            <div>
              <h2 className="text-base font-semibold">Something went wrong.</h2>
              <p className="text-sm text-muted-foreground mt-1">
                A page on the dashboard hit an unexpected error. The agent
                itself is still running. Reset the view to recover, or
                reload the browser.
              </p>
            </div>
          </div>

          <pre className="text-xs font-mono bg-background/60 border border-border rounded p-3 overflow-x-auto">
            {error.message}
          </pre>

          <div className="flex gap-2 justify-end">
            <button
              onClick={() => window.location.reload()}
              className="text-sm px-3 py-1.5 rounded-md border border-border hover:bg-accent"
            >
              Reload
            </button>
            <button
              onClick={this.reset}
              className="text-sm px-3 py-1.5 rounded-md border border-primary bg-primary/10 hover:bg-primary/20 inline-flex items-center gap-1.5"
            >
              <RefreshCcw className="h-3.5 w-3.5" />
              Reset view
            </button>
          </div>
        </div>
      </div>
    );
  }
}
