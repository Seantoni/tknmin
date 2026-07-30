/**
 * The compact window: what each source has used, what is left, and when it
 * comes back — and nothing else, because at this size anything more would push
 * that answer off the screen. The dashboard is one click away for the rest.
 *
 * Every allowance window gets a line, not just the one that binds first: a
 * 5-hour session and the week that contains it run down at different rates, and
 * Cursor's own models cannot spend what its other models have left. A source
 * with more than one window keeps them under its own name, so two lines about
 * Cursor read as Cursor's two pools rather than as two separate tools. A source
 * with a single window needs no such heading and gets one line.
 *
 * How many lines that adds up to depends on what the sources report — and they
 * differ by plan — so the window is sized from the rendered content rather than
 * guessed at.
 */

import { useEffect, useRef } from "react";

import { fitMiniWindowHeight, showDashboardWindow } from "../api/window";
import {
  describeQuotaResetCompact,
  describeSourceFreshness,
  describeQuotaWindows,
  formatPercentTenths,
  quotaGroups,
  quotaRemainingTenths,
  quotaRowKey,
  quotaWindowTag,
} from "../format";
import { useQuotas } from "../hooks/useQuotas";
import { SOURCE_COLOR, SOURCE_LABEL } from "../theme";
import type { SourceApp, UsageQuota } from "../domain/usage";

export function MiniView() {
  const { status, quotas, health, error } = useQuotas();
  const groups = quotaGroups(quotas);
  const panel = useRef<HTMLDivElement>(null);

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
      ) : groups.length === 0 ? (
        <p className="mini-empty">
          {status === "loading" ? "reading allowances…" : "no source reports an allowance yet"}
        </p>
      ) : (
        <ul className="mini-list">
          {groups.map(({ sourceApp, windows }) => {
            // One line of freshness per source, because at this size a
            // percentage with no age is indistinguishable from a percentage
            // that stopped updating an hour ago.
            const sourceHealth = health.find((each) => each.sourceApp === sourceApp);
            return (
            <li
              key={sourceApp}
              className="mini-group"
              title={sourceHealth === undefined ? undefined : describeSourceFreshness(sourceHealth)}
            >
              {windows.length === 1 ? (
                <Window
                  quota={windows[0]}
                  quotas={quotas}
                  name={SOURCE_LABEL[sourceApp]}
                  withDot
                />
              ) : (
                <>
                  <p className="mini-source">
                    <span className="dot" style={{ background: SOURCE_COLOR[sourceApp] }} />
                    {SOURCE_LABEL[sourceApp]}
                  </p>
                  <ul className="mini-pools">
                    {windows.map((quota) => (
                      <li key={quotaRowKey(quota)}>
                        <Window quota={quota} quotas={quotas} name={poolName(quota, sourceApp)} />
                      </li>
                    ))}
                  </ul>
                </>
              )}
              {sourceHealth?.awaitingUpstream === true && (
                // Cursor detects the activity long before it publishes what it
                // cost. Saying so beats implying the allowance is untouched.
                <p className="mini-note">activity detected · awaiting billing data</p>
              )}
            </li>
          );
          })}
        </ul>
      )}
    </div>
  );
}

/**
 * One allowance window: what it is called, how much is left, how much is gone,
 * and when it resets. The tooltip carries every window of the same source, so
 * one row can answer a question about its neighbours.
 *
 * The dot marks a source, so a row standing in for a whole source carries one
 * and a pool nested under a heading does not — its heading already did.
 */
function Window({
  quota,
  quotas,
  name,
  withDot = false,
}: {
  quota: UsageQuota;
  quotas: UsageQuota[];
  name: string;
  withDot?: boolean;
}) {
  const tag = quotaWindowTag(quota.windowMinutes);
  return (
    <div
      className="mini-row"
      title={describeQuotaWindows(quotas.filter((each) => each.sourceApp === quota.sourceApp))}
    >
      <span className="mini-line">
        {withDot && <span className="dot" style={{ background: SOURCE_COLOR[quota.sourceApp] }} />}
        <span className="mini-name">{name}</span>
        {name !== tag && <span className="mini-window">{tag}</span>}
        <span className="num mini-left">
          {formatPercentTenths(quotaRemainingTenths(quota))} left
        </span>
      </span>
      <span className="mini-line mini-detail">
        <span className="mini-used">{formatPercentTenths(quota.usedPercentTenths)} used</span>
        <span className="num mini-reset">{describeQuotaResetCompact(quota.resetsAt)}</span>
      </span>
      <span className="mini-bar">
        <span
          className="mini-bar-fill"
          style={{
            width: `${quota.usedPercentTenths / 10}%`,
            background: SOURCE_COLOR[quota.sourceApp],
          }}
        />
      </span>
    </div>
  );
}

/**
 * What a pool is called under its source's heading.
 *
 * "Cursor Models" below the word "cursor" repeats itself, so the source's name
 * gives way to "own" — the distinction the two pools actually draw. A pool the
 * source does not name is known by its window instead, which is what separates
 * Claude's session from the week around it.
 */
function poolName(quota: UsageQuota, sourceApp: SourceApp): string {
  if (quota.label === null) return quotaWindowTag(quota.windowMinutes);

  const label = quota.label.toLowerCase();
  const source = SOURCE_LABEL[sourceApp];
  return label.startsWith(`${source} `) ? `own ${label.slice(source.length + 1)}` : label;
}
