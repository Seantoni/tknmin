import type { GroupSummary, SourceApp, UsageQuota, UsageSummary } from "../domain/usage";
import {
  UNKNOWN,
  describeQuotaWindows,
  formatCostTotal,
  formatPercent,
  formatPercentTenths,
  formatTokens,
  quotaRemainingTenths,
  share,
  tightestQuotaPerSource,
} from "../format";
import { SOURCE_COLOR, SOURCE_LABEL } from "../theme";

interface SourceStripProps {
  /** Always the unfiltered summary, so the bars stay comparable. */
  overview: UsageSummary;
  quotas: UsageQuota[];
  selected: SourceApp | null;
  onSelect: (source: SourceApp) => void;
}

function isSourceApp(key: string | null): key is SourceApp {
  return key !== null && key in SOURCE_COLOR;
}

/**
 * Usage per source as a proportional bar, and the dashboard's filter control.
 *
 * The bars are drawn from the unfiltered totals even while a filter is on, so
 * selecting a source never destroys the comparison that justified selecting it.
 */
export function SourceStrip({ overview, quotas, selected, onSelect }: SourceStripProps) {
  const whole = overview.totals.displayTotal.tokens;

  return (
    <ul className="strip">
      {overview.bySource.map((group: GroupSummary) => {
        if (!isSourceApp(group.key)) return null;
        const source = group.key;
        const sourceQuotas = quotas.filter((candidate) => candidate.sourceApp === source);
        const quota = tightestQuotaPerSource(sourceQuotas)[0];
        // Cursor's recent local token fields are zero because its client-side
        // backfill is best-effort. Its server-reported billing-cycle meter is
        // the meaningful source-row metric; other sources retain token share.
        const showQuota = source === "cursor" && quota !== undefined;
        const tokenPercent = share(group.totals.displayTotal.tokens, whole);
        const percent = showQuota
          ? quota.usedPercentTenths / 10
          : tokenPercent;
        const isSelected = selected === source;
        const isDimmed = selected !== null && !isSelected;

        if (showQuota && sourceQuotas.length > 1) {
          return (
            <li key={source} className="cursor-pools">
              <button
                type="button"
                className={`strip-row${isSelected ? " is-selected" : ""}${isDimmed ? " is-dimmed" : ""}`}
                onClick={() => onSelect(source)}
                aria-pressed={isSelected}
                title={isSelected ? "show every source" : `show only ${SOURCE_LABEL[source]}`}
              >
                <span className="dot" style={{ background: SOURCE_COLOR[source] }} />
                <span className="strip-name">{SOURCE_LABEL[source]}</span>
                <span className="bar">
                  <span
                    className="bar-fill"
                    style={{ width: `${tokenPercent}%`, background: SOURCE_COLOR[source] }}
                  />
                </span>
                <span className="num strip-share">{formatPercent(tokenPercent)}</span>
                <span className="num strip-tokens">
                  {formatTokens(group.totals.displayTotal.tokens)}
                </span>
                <span className="num strip-cost">
                  {group.totals.cost.byCurrency.length === 0
                    ? UNKNOWN
                    : formatCostTotal(group.totals.cost)}
                </span>
              </button>
              {sourceQuotas.map((pool, index) => {
                const poolPercent = pool.usedPercentTenths / 10;
                return (
                  <button
                    key={pool.label ?? index}
                    type="button"
                    className={`strip-row${isSelected ? " is-selected" : ""}${isDimmed ? " is-dimmed" : ""}`}
                    onClick={() => onSelect(source)}
                    aria-pressed={isSelected}
                    title={describeQuotaWindows([pool])}
                  >
                    <span className="dot is-faint" style={{ background: SOURCE_COLOR[source] }} />
                    <span className="strip-name">
                      {pool.label?.toLowerCase()}
                    </span>
                    <span className="bar">
                      <span
                        className="bar-fill"
                        style={{
                          width: `${poolPercent}%`,
                          background: SOURCE_COLOR[source],
                        }}
                      />
                    </span>
                    <span className="num strip-share">{formatPercent(poolPercent)} used</span>
                    <span className="num strip-tokens">
                      {formatPercentTenths(quotaRemainingTenths(pool))} left
                    </span>
                    <span className="num strip-cost">{UNKNOWN}</span>
                  </button>
                );
              })}
            </li>
          );
        }

        return (
          <li key={source}>
            <button
              type="button"
              className={`strip-row${isSelected ? " is-selected" : ""}${isDimmed ? " is-dimmed" : ""}`}
              onClick={() => onSelect(source)}
              aria-pressed={isSelected}
              title={
                showQuota
                  ? describeQuotaWindows(quotas.filter((candidate) => candidate.sourceApp === source))
                  : isSelected
                    ? "show every source"
                    : `show only ${SOURCE_LABEL[source]}`
              }
            >
              <span className="dot" style={{ background: SOURCE_COLOR[source] }} />
              <span className="strip-name">{SOURCE_LABEL[source]}</span>
              <span className="bar">
                <span
                  className="bar-fill"
                  style={{ width: `${percent}%`, background: SOURCE_COLOR[source] }}
                />
              </span>
              <span className="num strip-share">
                {showQuota ? `${formatPercent(percent)} used` : formatPercent(percent)}
              </span>
              <span className="num strip-tokens">
                {showQuota
                  ? `${formatPercentTenths(quotaRemainingTenths(quota))} left`
                  : formatTokens(group.totals.displayTotal.tokens)}
              </span>
              <span className="num strip-cost">
                {group.totals.cost.byCurrency.length === 0
                  ? UNKNOWN
                  : formatCostTotal(group.totals.cost)}
              </span>
            </button>
          </li>
        );
      })}
    </ul>
  );
}
