import { useState } from "react";

import type { GroupSummary } from "../domain/usage";
import { UNKNOWN, formatCostTotal, formatPercent, formatTokens, share } from "../format";

interface ModelBreakdownProps {
  groups: GroupSummary[];
}

/**
 * Per-model usage, collapsed by default.
 *
 * The source split answers most questions; models are a second question, so
 * they cost one click rather than a screenful.
 */
export function ModelBreakdown({ groups }: ModelBreakdownProps) {
  const [isOpen, setIsOpen] = useState(false);
  const whole = groups.reduce((sum, group) => sum + group.totals.displayTotal.tokens, 0);

  if (groups.length === 0) return null;

  return (
    <section className="block">
      <button
        type="button"
        className="disclosure"
        onClick={() => setIsOpen((open) => !open)}
        aria-expanded={isOpen}
      >
        <span className={`chevron${isOpen ? " is-open" : ""}`} aria-hidden="true" />
        <span className="label">models</span>
        <span className="count">{groups.length}</span>
      </button>

      {isOpen && (
        <ul className="strip">
          {groups.map((group) => {
            const percent = share(group.totals.displayTotal.tokens, whole);
            return (
              <li key={group.key ?? "__unreported__"}>
                <div className="strip-row is-static">
                  <span className="dot is-empty" aria-hidden="true" />
                  <span className="strip-name">
                    {group.key ?? "unreported"}
                    {group.key === null && <span className="tag">no model in log</span>}
                  </span>
                  <span className="bar">
                    <span className="bar-fill is-neutral" style={{ width: `${percent}%` }} />
                  </span>
                  <span className="num strip-share">{formatPercent(percent)}</span>
                  <span className="num strip-tokens">
                    {formatTokens(group.totals.displayTotal.tokens)}
                  </span>
                  <span className="num strip-cost">
                    {group.totals.cost.byCurrency.length === 0
                      ? UNKNOWN
                      : formatCostTotal(group.totals.cost)}
                  </span>
                </div>
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}
