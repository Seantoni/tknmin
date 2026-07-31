/**
 * Allowance rows — the one place the shape of "what is left and will it last"
 * is decided, rendered identically by the compact window and the dashboard.
 *
 * Every allowance window gets a row, not just the one that binds first: a
 * 5-hour session and the week that contains it run down at different rates,
 * and Cursor's own models cannot spend what its other models have left. A
 * source with more than one window keeps them under its own name, so two rows
 * about Cursor read as Cursor's two pools rather than as two separate tools. A
 * source with a single window needs no such heading and gets one row.
 *
 * A stacked row is always two lines around one bar, whatever the sources
 * report:
 *
 * ```text
 * ● codex  week                31% left
 * ▓▓▓▓▓▓▓░░░░░░░░░░░░░░░░░░░░░░░░░░░░░
 * out in 14h · 5d short  resets Aug 5 08:06
 * ```
 *
 * The first line is position, the bar is the same fact drawn, and the last
 * line is the only thing position cannot say: whether the current pace reaches
 * the reset, and when that reset is. The two sit side by side deliberately —
 * the gap between them is the whole point.
 *
 * The horizontal layout says the same on a single line — name, bar, position,
 * pace, reset left to right — for the compact window's one-row mode. Nothing
 * is dropped; the lines are only rearranged.
 *
 * Both callers pass their own sizing through the surrounding class, so the
 * dashboard can breathe without the design drifting from the panel's.
 */

import {
  describeEvenPace,
  describePaceCompact,
  describePaceDetail,
  describePaceGap,
  describeProjection,
  describeQuotaResetCompact,
  describeQuotaWindows,
  describeSourceFreshness,
  formatPercentTenths,
  paceForQuota,
  projectionForQuota,
  quotaEvenPacePercent,
  quotaGroups,
  quotaLiveRemainingTenths,
  quotaRowKey,
  quotaUsedTenths,
  quotaWindowTag,
} from "../format";
import { SOURCE_COLOR, SOURCE_LABEL } from "../theme";
import type { QuotaProjection, WindowPace } from "../domain/pace";
import type { SourceApp, SourceSyncHealth, UsageQuota } from "../domain/usage";

interface QuotaRowsProps {
  quotas: UsageQuota[];
  pace: WindowPace[];
  /**
   * Confirmed readings carried forward. Empty is normal — sources that publish
   * continuously need no projection, and a source with too little history has
   * not earned one yet.
   */
  projections: QuotaProjection[];
  health: SourceSyncHealth[];
  /** Ticking clock, so ages and countdowns in the tooltips stay live. */
  now: Date;
  /**
   * "stacked" is two lines around a bar per allowance; "horizontal" puts the
   * name, the bar, and every number on a single row. The compact window
   * chooses; the dashboard leaves the default alone.
   */
  layout?: "stacked" | "horizontal";
}

export function QuotaRows({ quotas, pace, projections, health, now, layout }: QuotaRowsProps) {
  const horizontal = layout === "horizontal";
  return (
    <ul className={horizontal ? "q-list is-horizontal" : "q-list"}>
      {quotaGroups(quotas).map(({ sourceApp, windows }) => {
        // One line of freshness per source, because a percentage with no age is
        // indistinguishable from a percentage that stopped updating an hour ago.
        const sourceHealth = health.find((each) => each.sourceApp === sourceApp);
        return (
          <li
            key={sourceApp}
            className="q-group"
            title={sourceHealth === undefined ? undefined : describeSourceFreshness(sourceHealth)}
          >
            {windows.length === 1 ? (
              <Row
                quota={windows[0]}
                quotas={quotas}
                pace={paceForQuota(pace, windows[0])}
                projection={projectionForQuota(projections, windows[0])}
                now={now}
                name={SOURCE_LABEL[sourceApp]}
                windowTag={quotaWindowTag(windows[0].windowMinutes)}
                withDot
                horizontal={horizontal}
              />
            ) : horizontal ? (
              // No heading to nest under here: the source's name rides inside
              // each of its rows, so every pool is still one complete row.
              <ul className="q-pools is-horizontal">
                {windows.map((quota) => {
                  const pool = poolName(quota, sourceApp);
                  const tag = quotaWindowTag(quota.windowMinutes);
                  return (
                    <li key={quotaRowKey(quota)}>
                      <Row
                        quota={quota}
                        quotas={quotas}
                        pace={paceForQuota(pace, quota)}
                        projection={projectionForQuota(projections, quota)}
                        now={now}
                        name={`${SOURCE_LABEL[sourceApp]} ${pool}`}
                        windowTag={pool === tag ? null : tag}
                        withDot
                        horizontal
                      />
                    </li>
                  );
                })}
              </ul>
            ) : (
              <>
                <p className="q-source">
                  <span className="dot" style={{ background: SOURCE_COLOR[sourceApp] }} />
                  {SOURCE_LABEL[sourceApp]}
                </p>
                <ul className="q-pools">
                  {windows.map((quota) => {
                    const pool = poolName(quota, sourceApp);
                    const tag = quotaWindowTag(quota.windowMinutes);
                    return (
                      <li key={quotaRowKey(quota)}>
                        <Row
                          quota={quota}
                          quotas={quotas}
                          pace={paceForQuota(pace, quota)}
                          projection={projectionForQuota(projections, quota)}
                          now={now}
                          name={pool}
                          windowTag={pool === tag ? null : tag}
                        />
                      </li>
                    );
                  })}
                </ul>
              </>
            )}
            {sourceHealth?.awaitingUpstream === true && (
              // Cursor detects the activity long before it publishes what it
              // cost. Saying so beats implying the allowance is untouched.
              <p className="q-note">activity detected · awaiting billing data</p>
            )}
          </li>
        );
      })}
    </ul>
  );
}

/**
 * The dot marks a source, so a row standing in for a whole source carries one
 * and a pool nested under a heading does not — its heading already did. In the
 * horizontal layout there is no heading, so every row carries its dot.
 *
 * `windowTag` is null when the name already says the window ("claude code 5h"
 * needs no second "5h").
 */
function Row({
  quota,
  quotas,
  pace,
  projection,
  now,
  name,
  windowTag,
  withDot = false,
  horizontal = false,
}: {
  quota: UsageQuota;
  quotas: UsageQuota[];
  pace: WindowPace | undefined;
  projection: QuotaProjection | undefined;
  now: Date;
  name: string;
  windowTag: string | null;
  withDot?: boolean;
  horizontal?: boolean;
}) {
  const verdict = pace === undefined ? null : describePaceCompact(pace);
  const gap = pace === undefined ? null : describePaceGap(pace);
  const evenPace = quotaEvenPacePercent(quota, now);
  // The tooltip carries every window of the same source, so one row can answer
  // a question about its neighbours, plus the caveats the row leaves out —
  // including, when the headline is derived, what was measured and what was
  // inferred.
  const tooltip = [
    describeQuotaWindows(quotas.filter((each) => each.sourceApp === quota.sourceApp)),
    projection === undefined ? null : describeProjection(projection, now),
    describeEvenPace(quota, now),
    pace === undefined ? null : describePaceDetail(pace, now),
  ]
    .filter((part): part is string => part !== null)
    .join("\n\n");

  // The bar is drawn in two segments: what the source confirmed, solid, and
  // what has been inferred since, faded on the end of it. Reading the boundary
  // is how the eye separates a measurement from an estimate without a legend.
  const confirmedPercent = Math.min(100, quota.usedPercentTenths / 10);
  const projectedPercent = Math.min(100, quotaUsedTenths(quota, projection) / 10);

  const bar = (
    <span className="q-bar">
      {projection !== undefined && (
        <span
          className="q-bar-fill is-projected"
          style={{
            width: `${projectedPercent}%`,
            background: SOURCE_COLOR[quota.sourceApp],
          }}
        />
      )}
      <span
        className="q-bar-fill"
        style={{
          width: `${confirmedPercent}%`,
          background: SOURCE_COLOR[quota.sourceApp],
        }}
      />
      {/* Where even spending would have reached by now. The fill's position
          against this mark is the whole judgement, drawn rather than worded. */}
      {evenPace !== null && <span className="q-bar-target" style={{ left: `${evenPace}%` }} />}
    </span>
  );

  const left = (
    <span className="num q-left">
      {/* The tilde is the whole disclosure at a glance: this number moved
          without the source having said so. */}
      {projection !== undefined && <span className="q-approx">≈</span>}
      {formatPercentTenths(quotaLiveRemainingTenths(quota, projection))} left
    </span>
  );

  if (horizontal) {
    // Everything the stacked row says on three lines, said on one: identity
    // left, the bar stretching over whatever width is left, then the numbers.
    return (
      <div className="q-row is-horizontal" title={tooltip}>
        <span className="q-line">
          {withDot && (
            <span className="dot" style={{ background: SOURCE_COLOR[quota.sourceApp] }} />
          )}
          <span className="q-name">{name}</span>
          {windowTag !== null && <span className="q-window">{windowTag}</span>}
          {bar}
          {left}
          {verdict !== null && <span className={`q-verdict is-${pace?.state}`}>{verdict}</span>}
          {gap !== null && <span className="q-gap">· {gap}</span>}
          <span className="num q-reset">{describeQuotaResetCompact(quota.resetsAt)}</span>
        </span>
      </div>
    );
  }

  return (
    <div className="q-row" title={tooltip}>
      <span className="q-line">
        {withDot && <span className="dot" style={{ background: SOURCE_COLOR[quota.sourceApp] }} />}
        <span className="q-name">{name}</span>
        {windowTag !== null && <span className="q-window">{windowTag}</span>}
        {left}
      </span>
      {bar}
      <span className="q-line q-detail">
        {verdict !== null && <span className={`q-verdict is-${pace?.state}`}>{verdict}</span>}
        {gap !== null && <span className="q-gap">· {gap}</span>}
        <span className="num q-reset">{describeQuotaResetCompact(quota.resetsAt)}</span>
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
