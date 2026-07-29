import { useState } from "react";

import type { TokenField, UsageRecord } from "../domain/usage";
import {
  UNKNOWN,
  describeQuality,
  describeTimestamp,
  describeTotalRule,
  formatMoneyAmount,
  formatShortEventTime,
  formatTokenField,
  formatTokens,
  orUnknown,
} from "../format";
import { SOURCE_COLOR, SOURCE_LABEL } from "../theme";

/** A value that may carry a caveat, marked by a dotted underline. */
function Value({ children, note }: { children: React.ReactNode; note: string | null }) {
  if (note === null) return <>{children}</>;
  return (
    <span className="has-note" title={note}>
      {children}
    </span>
  );
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="field">
      <dt>{label}</dt>
      <dd>{children}</dd>
    </div>
  );
}

function TokenDetail({ label, field }: { label: string; field: TokenField }) {
  return (
    <Field label={label}>
      <span className="num">
        <Value note={describeQuality(field.quality)}>{formatTokenField(field)}</Value>
      </span>
    </Field>
  );
}

/** Everything the compact row leaves out. */
function EventDetail({ record }: { record: UsageRecord }) {
  const timestampNote = describeTimestamp(record.timestampInterpretation);

  return (
    <dl className="detail">
      <TokenDetail label="input" field={record.tokens.input} />
      <TokenDetail label="output" field={record.tokens.output} />
      <TokenDetail label="cached input" field={record.tokens.cachedInput} />
      <TokenDetail label="reasoning" field={record.tokens.reasoning} />

      <Field label="total">
        {record.displayTotal === null ? (
          UNKNOWN
        ) : (
          <>
            <span className="num">{formatTokens(record.displayTotal.tokens)}</span>
            <span className="hint">{describeTotalRule(record.displayTotal.rule)}</span>
          </>
        )}
      </Field>
      <Field label="cost">
        {record.cost.amount === null ? (
          UNKNOWN
        ) : (
          <>
            <span className="num">{formatMoneyAmount(record.cost.amount, 4)}</span>
            <span className="hint">
              {record.cost.status === "reported_by_source"
                ? "reported by the source"
                : `priced locally · ${orUnknown(record.cost.pricingVersion)}`}
            </span>
          </>
        )}
      </Field>

      <Field label="provider">{orUnknown(record.provider)}</Field>
      <Field label="project">{orUnknown(record.project)}</Field>
      <Field label="session">{orUnknown(record.sessionId)}</Field>

      <Field label="logged at">
        <Value note={timestampNote}>
          <span className="num">{orUnknown(record.rawTimestamp)}</span>
        </Value>
      </Field>

      <Field label="event id">
        <span className="num truncate">{orUnknown(record.sourceEventId)}</span>
      </Field>
      <Field label="read by">
        <span className="num truncate">
          {record.provenance.adapterId} {record.provenance.adapterVersion}
        </span>
      </Field>
    </dl>
  );
}

interface EventListProps {
  records: UsageRecord[];
}

/** Enough to see the shape of recent activity without scrolling. */
const COLLAPSED_COUNT = 8;

/**
 * The event log: five columns, and everything else one click away.
 *
 * One row opens at a time, and only the most recent handful are listed until
 * asked otherwise, so the list never becomes a wall of detail.
 */
export function EventList({ records }: EventListProps) {
  const [openId, setOpenId] = useState<string | null>(null);
  const [showAll, setShowAll] = useState(false);

  if (records.length === 0) {
    return <p className="empty">No events match this filter.</p>;
  }

  const visible = showAll ? records : records.slice(0, COLLAPSED_COUNT);
  const hidden = records.length - visible.length;

  return (
    <>
      <ul className="events">
        {visible.map((record) => {
          const isOpen = openId === record.id;
          const timestampNote = describeTimestamp(record.timestampInterpretation);
          // The compact row shows only input's caveat; the rest live in the detail.
          const countNote = describeQuality(record.tokens.input.quality);

          return (
            <li key={record.id} className={isOpen ? "is-open" : undefined}>
              <button
                type="button"
                className="event-row"
                onClick={() => setOpenId(isOpen ? null : record.id)}
                aria-expanded={isOpen}
              >
                <span className="chevron" aria-hidden="true" />
                <span className="num event-time">
                  <Value note={timestampNote}>{formatShortEventTime(record)}</Value>
                </span>
                <span className="event-source">
                  <span className="dot" style={{ background: SOURCE_COLOR[record.sourceApp] }} />
                  {SOURCE_LABEL[record.sourceApp]}
                </span>
                <span className="event-model truncate">{orUnknown(record.model)}</span>
                <span className="num event-tokens">
                  <Value note={countNote}>
                    {record.displayTotal === null
                      ? UNKNOWN
                      : formatTokens(record.displayTotal.tokens)}
                  </Value>
                </span>
                <span className="num event-cost">
                  {record.cost.amount === null
                    ? UNKNOWN
                    : formatMoneyAmount(record.cost.amount, 2)}
                </span>
              </button>

              {isOpen && <EventDetail record={record} />}
            </li>
          );
        })}
      </ul>

      {(hidden > 0 || showAll) && (
        <button type="button" className="more" onClick={() => setShowAll((all) => !all)}>
          {showAll ? "show less" : `show ${hidden} more`}
        </button>
      )}
    </>
  );
}
