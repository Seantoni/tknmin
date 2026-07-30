/**
 * The compact window: what each source has used, what is left, and whether the
 * current pace reaches the reset — and nothing else, because at this size
 * anything more would push that answer off the screen. The dashboard is one
 * click away for the rest.
 *
 * The rows themselves are [`QuotaRows`], shared with the dashboard so the two
 * cannot drift apart. What belongs to this file is the panel: its chrome, its
 * drag region, and its height.
 *
 * How many rows that adds up to depends on what the sources report — and they
 * differ by plan — so the window is sized from the rendered content rather
 * than guessed at.
 */

import { useEffect, useRef } from "react";

import { fitMiniWindowHeight, showDashboardWindow } from "../api/window";
import { QuotaRows } from "./QuotaRows";
import { useQuotas } from "../hooks/useQuotas";
import { useNow } from "../hooks/useNow";

export function MiniView() {
  const { status, quotas, pace, projections, health, error } = useQuotas();
  const panel = useRef<HTMLDivElement>(null);
  const now = useNow();

  // Follow the panel's own height rather than recomputing it here: a wrapped
  // label or a source that starts reporting a second pool changes it too.
  useEffect(() => {
    const element = panel.current;
    if (element === null) return;

    const fit = () => {
      const height = Math.ceil(element.getBoundingClientRect().height);
      if (height > 0) void fitMiniWindowHeight(height);
    };

    fit();
    const observer = new ResizeObserver(fit);
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  return (
    <div className="mini" ref={panel}>
      {/* Undecorated windows have nothing else to drag by. */}
      <header className="mini-top" data-tauri-drag-region>
        <span className="mini-brand">tokens</span>
        <button
          type="button"
          className="ghost small"
          title="back to the dashboard"
          onClick={() => void showDashboardWindow()}
        >
          expand
        </button>
      </header>

      {status === "error" && error !== null ? (
        <p className="mini-empty">{error.message}</p>
      ) : quotas.length === 0 ? (
        <p className="mini-empty">
          {status === "loading" ? "reading allowances…" : "no source reports an allowance yet"}
        </p>
      ) : (
        <QuotaRows
          quotas={quotas}
          pace={pace}
          projections={projections}
          health={health}
          now={now}
        />
      )}
    </div>
  );
}
