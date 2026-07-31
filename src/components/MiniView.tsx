/**
 * The compact window: what each source has used, what is left, and whether the
 * current pace reaches the reset — and nothing else, because at this size
 * anything more would push that answer off the screen. The dashboard is one
 * click away for the rest.
 *
 * The rows themselves are [`QuotaRows`], shared with the dashboard so the two
 * cannot drift apart, in one of three layouts the user cycles through here:
 * stacked, two lines around a bar in a narrow panel; horizontal, one complete
 * row per allowance in a wide one; or grid, every block side by side in a
 * single row.
 * What belongs to this file is the panel: its chrome, its drag region, its
 * layout toggle, and its size.
 *
 * How tall the panel is depends on what the sources report — and they differ
 * by plan — so the window is sized from the rendered content rather than
 * guessed at. How wide it is depends only on the layout.
 */

import { useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";

import { fetchOptions, saveOptions, OPTIONS_CHANGED } from "../api/options";
import {
  fitMiniWindowSize,
  showDashboardWindow,
  miniGridWidth,
  MINI_WIDTH,
  MINI_WIDE_WIDTH,
} from "../api/window";
import type { AppOptions, MiniLayout } from "../domain/options";
import { QuotaRows } from "./QuotaRows";
import { useQuotas } from "../hooks/useQuotas";
import { useNow } from "../hooks/useNow";

/** The toggle walks this ring, one layout per press: what the button
 *  promises next, and what the window becomes. */
const NEXT: Record<MiniLayout, { layout: MiniLayout; label: string; title: string }> = {
  stacked: {
    layout: "horizontal",
    label: "one row",
    title: "each allowance on one horizontal row",
  },
  horizontal: {
    layout: "grid",
    label: "grid",
    title: "the blocks side by side, all in one row",
  },
  grid: {
    layout: "stacked",
    label: "stacked",
    title: "back to stacked rows",
  },
};

function widthFor(layout: MiniLayout, cells: number): number {
  switch (layout) {
    case "horizontal":
      return MINI_WIDE_WIDTH;
    case "grid":
      return miniGridWidth(cells);
    case "stacked":
      return MINI_WIDTH;
  }
}

export function MiniView() {
  const { status, quotas, pace, projections, health, error } = useQuotas();
  const [layout, setLayout] = useState<MiniLayout>("stacked");
  const panel = useRef<HTMLDivElement>(null);
  const now = useNow();

  // The layout is a saved option, so it survives restarts and reaches the
  // dashboard's settings. Load it once, then follow every save — including
  // this window's own, which comes back around as the same event.
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;

    void listen<AppOptions>(OPTIONS_CHANGED, (event) => {
      if (!cancelled) setLayout(event.payload.miniLayout);
    }).then((off) => {
      if (cancelled) off();
      else unlisten = off;
    });

    void fetchOptions()
      .then((options) => {
        if (!cancelled) setLayout(options.miniLayout);
      })
      .catch(() => {
        // Stacked is the default the window already shows; a failed read is
        // no reason to leave the panel unusable.
      });

    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  // Follow the panel's own height rather than recomputing it here: a wrapped
  // label or a source that starts reporting a second pool changes it too.
  // The layout re-runs this because it decides the width — and in grid the
  // width also follows how many blocks the sources currently report.
  useEffect(() => {
    const element = panel.current;
    if (element === null) return;

    const width = widthFor(layout, quotas.length);
    const fit = () => {
      const height = Math.ceil(element.getBoundingClientRect().height);
      if (height > 0) void fitMiniWindowSize(width, height);
    };

    fit();
    const observer = new ResizeObserver(fit);
    observer.observe(element);
    // The observer only fires when the panel itself changes size, so a fit
    // made mid-reflow can leave the window stuck at a transient height the
    // panel then never revisits. Re-fit a few times while the layout settles.
    const settling = [50, 150, 400].map((ms) => window.setTimeout(fit, ms));
    return () => {
      observer.disconnect();
      for (const timer of settling) window.clearTimeout(timer);
    };
  }, [layout, quotas.length]);

  const toggleLayout = () => {
    const next = NEXT[layout].layout;
    setLayout(next);
    // Read-then-write, so the rest of the options survive even if another
    // window moved them since this one loaded.
    void (async () => {
      try {
        const current = await fetchOptions();
        await saveOptions({ ...current, miniLayout: next });
      } catch {
        setLayout(layout);
      }
    })();
  };

  return (
    <div className="mini" ref={panel}>
      {/* Undecorated windows have nothing else to drag by. */}
      <header className="mini-top" data-tauri-drag-region>
        <span className="mini-brand">tokens</span>
        <span className="mini-actions">
          <button
            type="button"
            className="ghost small"
            title={NEXT[layout].title}
            onClick={toggleLayout}
          >
            {NEXT[layout].label}
          </button>
          <button
            type="button"
            className="ghost small"
            title="back to the dashboard"
            onClick={() => void showDashboardWindow()}
          >
            expand
          </button>
        </span>
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
          layout={layout}
        />
      )}
    </div>
  );
}
