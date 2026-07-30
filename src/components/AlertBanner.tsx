import type { ThresholdAlert } from "../domain/notifications";
import {
  describeQuotaWindow,
  formatPercentTenths,
  formatQuotaReset,
} from "../format";
import { SOURCE_COLOR, SOURCE_LABEL } from "../theme";

type AlertBannerProps = {
  alerts: ThresholdAlert[];
  onContinue: (dedupeKey: string) => void;
  onCreateHandoff: (dedupeKey: string) => void;
  handoffCopiedKey: string | null;
};

/** The crossed line, as the banner says it. Exhaustive on the metric. */
function alertHeadline(alert: ThresholdAlert): string {
  switch (alert.metric) {
    case "remaining_percent":
      return `${formatPercentTenths(alert.remainingPercentTenths)} left`;
    case "minutes_until_reset":
      return `resets in ${alert.minutesUntilReset} min`;
    case "projected_exhaustion":
      return alert.shortfallMinutes === null
        ? "projected to run out early"
        : `projected to run out ${alert.shortfallMinutes} min early`;
  }
}

export function AlertBanner({
  alerts,
  onContinue,
  onCreateHandoff,
  handoffCopiedKey,
}: AlertBannerProps) {
  if (alerts.length === 0) return null;

  return (
    <div className="alert-stack" role="region" aria-label="usage alerts">
      {alerts.map((alert) => (
        <article key={alert.dedupeKey} className="alert-banner">
          <div className="alert-main">
            <span className="dot" style={{ background: SOURCE_COLOR[alert.sourceApp] }} />
            <div className="alert-copy">
              <p className="alert-title">
                {SOURCE_LABEL[alert.sourceApp]} · {alertHeadline(alert)}{" "}
                {describeQuotaWindow(alert.windowMinutes)}
              </p>
              <p className="alert-meta">
                resets {formatQuotaReset(alert.resetsAt)}
                {handoffCopiedKey === alert.dedupeKey ? " · handoff prompt copied" : ""}
              </p>
            </div>
          </div>
          <div className="alert-actions">
            <button
              type="button"
              className="ghost small"
              onClick={() => onCreateHandoff(alert.dedupeKey)}
            >
              create handoff
            </button>
            <button
              type="button"
              className="ghost small"
              onClick={() => onContinue(alert.dedupeKey)}
            >
              continue
            </button>
          </div>
        </article>
      ))}
    </div>
  );
}
