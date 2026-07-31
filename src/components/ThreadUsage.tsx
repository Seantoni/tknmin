import { useState } from "react";

import type { ThreadSummary, ThreadUsageReport } from "../domain/usage";
import {
  UNKNOWN,
  formatCostTotal,
  formatInstantSpan,
  formatPercent,
  formatTokens,
  share,
} from "../format";
import { SOURCE_COLOR, SOURCE_LABEL } from "../theme";

/**
 * What a thread is called. The session id is an opaque UUID, and prompts are
 * deliberately never stored, so the name is the project the thread ran in —
 * the one piece of metadata that reads like a name — with the source and its
 * span as context. The full id stays one hover away.
 */
function threadName(thread: ThreadSummary): string {
  return thread.project ?? `thread ${thread.sessionId.slice(0, 6)}`;
}

function threadTooltip(thread: ThreadSummary): string {
  const models =
    thread.models.length === 0 ? "no model in log" : thread.models.join(", ");
  return `${SOURCE_LABEL[thread.sourceApp]} · ${models}\nsession ${thread.sessionId}`;
}

interface ThreadUsageProps {
  report: ThreadUsageReport;
}

/**
 * Per-thread usage, collapsed by default like the model breakdown.
 *
 * Shares are against every thread-attributed record, not just the capped
 * list, so a row's bar means the same thing whether or not the list was cut.
 */
export function ThreadUsage({ report }: ThreadUsageProps) {
  const [isOpen, setIsOpen] = useState(false);
  const whole = report.totals.displayTotal.tokens;

  if (report.threadCount === 0) return null;

  const hidden = report.threadCount - report.threads.length;

  return (
    <section className="block">
      <button
        type="button"
        className="disclosure"
        onClick={() => setIsOpen((open) => !open)}
        aria-expanded={isOpen}
      >
        <span className={`chevron${isOpen ? " is-open" : ""}`} aria-hidden="true" />
        <span className="label">threads</span>
        <span className="count">{report.threadCount}</span>
      </button>

      {isOpen && (
        <>
          <ul className="strip">
            {report.threads.map((thread) => {
              const percent = share(thread.totals.displayTotal.tokens, whole);
              const span = formatInstantSpan(thread.firstEventAt, thread.lastEventAt);
              return (
                <li key={thread.key}>
                  <div className="strip-row is-static" title={threadTooltip(thread)}>
                    <span
                      className="dot"
                      style={{ background: SOURCE_COLOR[thread.sourceApp] }}
                      aria-hidden="true"
                    />
                    <span className="strip-name">
                      {threadName(thread)}
                      <span className="tag">
                        {SOURCE_LABEL[thread.sourceApp]}
                        {span !== null && ` · ${span}`}
                      </span>
                    </span>
                    <span className="bar">
                      <span className="bar-fill is-neutral" style={{ width: `${percent}%` }} />
                    </span>
                    <span className="num strip-share">{formatPercent(percent)}</span>
                    <span className="num strip-tokens">
                      {formatTokens(thread.totals.displayTotal.tokens)}
                    </span>
                    <span className="num strip-cost">
                      {thread.totals.cost.byCurrency.length === 0
                        ? UNKNOWN
                        : formatCostTotal(thread.totals.cost)}
                    </span>
                  </div>
                </li>
              );
            })}
          </ul>
          {(hidden > 0 || report.unattributedRecords > 0) && (
            <p className="strip-foot">
              {[
                hidden > 0 ? `showing ${report.threads.length} of ${report.threadCount}` : null,
                report.unattributedRecords > 0
                  ? `${formatTokens(report.unattributedRecords)} ${
                      report.unattributedRecords === 1 ? "event" : "events"
                    } not tied to a thread`
                  : null,
              ]
                .filter((note) => note !== null)
                .join(" · ")}
            </p>
          )}
        </>
      )}
    </section>
  );
}
