import type { UsageRecord, UsageTotals } from "../domain/usage";
import {
  UNKNOWN,
  describeTotalGaps,
  formatCostTotal,
  formatDateSpan,
  formatTokenTotal,
  formatTokens,
} from "../format";

interface StatProps {
  label: string;
  value: string;
  /** One quiet line of context. Kept to a few words. */
  detail: string | null;
  /** The full explanation, available on hover rather than on screen. */
  title?: string;
}

function Stat({ label, value, detail, title }: StatProps) {
  return (
    <div className="stat">
      <div className="stat-label">{label}</div>
      <div className="stat-value" title={title}>
        {value}
      </div>
      <div className="stat-detail">{detail ?? ""}</div>
    </div>
  );
}

interface StatBarProps {
  totals: UsageTotals;
  records: UsageRecord[];
}

/**
 * The headline figures: three numbers, each with one line of context.
 *
 * Gaps in the data are stated as a short detail line and explained in full on
 * hover, so an incomplete total never presents itself as complete.
 */
export function StatBar({ totals, records }: StatBarProps) {
  const span = formatDateSpan(records);
  const costGaps =
    totals.cost.recordsWithoutCost > 0
      ? `${formatTokens(totals.cost.recordsWithoutCost)} without cost`
      : null;

  return (
    <div className="stat-bar">
      <Stat
        label="tokens"
        value={formatTokenTotal(totals.displayTotal)}
        detail={`${formatTokenTotal(totals.input)} in · ${formatTokenTotal(totals.output)} out`}
        title={describeTotalGaps(totals.displayTotal) ?? undefined}
      />
      <Stat
        label="cost"
        value={totals.cost.byCurrency.length === 0 ? UNKNOWN : formatCostTotal(totals.cost)}
        detail={costGaps}
        title={costGaps === null ? undefined : "some records report no cost"}
      />
      <Stat label="events" value={formatTokens(totals.recordCount)} detail={span} />
    </div>
  );
}
