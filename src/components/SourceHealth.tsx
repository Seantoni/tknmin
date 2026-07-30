/**
 * Per-source freshness: what is being watched, how long ago it was read, and
 * what went wrong if anything did.
 *
 * This row exists because the numbers above it are no longer refreshed by the
 * user. When a person pressed "refresh" they knew exactly how old the screen
 * was; now that nothing is pressed, the screen has to say so itself — and it
 * has to keep saying so when a source fails, since the values stay on screen
 * as last-known-good rather than disappearing.
 *
 * Two ages, not one. "App synced" is when this app last read the source;
 * "source reported" is when the source says its data was true. Cursor
 * publishes billing well after the activity that caused it, so collapsing the
 * two into a single "updated 5s ago" would be a claim the data cannot support.
 */

import { describeSourceFreshness, describeSyncState, formatAge } from "../format";
import { SOURCE_COLOR, SOURCE_LABEL } from "../theme";
import type { AdapterInfo, SourceSyncHealth } from "../domain/usage";

interface SourceHealthProps {
  health: SourceSyncHealth[];
  /** Static facts, used to explain an offline state rather than call it a bug. */
  sources: AdapterInfo[];
}

export function SourceHealthRow({ health, sources }: SourceHealthProps) {
  if (health.length === 0) return null;

  const ordered = [...health].sort((left, right) =>
    SOURCE_LABEL[left.sourceApp].localeCompare(SOURCE_LABEL[right.sourceApp]),
  );

  return (
    <ul className="health">
      {ordered.map((source) => {
        const capability = sources.find((info) => info.sourceApp === source.sourceApp);
        const failed = source.state === "offline" || source.state === "error";
        return (
          <li
            key={source.sourceApp}
            className={`health-row is-${source.state}${source.awaitingUpstream ? " is-waiting" : ""}`}
            title={describeSourceFreshness(source)}
          >
            <span className="dot" style={{ background: SOURCE_COLOR[source.sourceApp] }} />
            <span className="health-name">{SOURCE_LABEL[source.sourceApp]}</span>
            <span className="health-state">{describeSyncState(source)}</span>
            <span className="num health-age">app synced {formatAge(source.appSyncedAt)}</span>
            <span className="num health-age">
              source reported {formatAge(source.sourceObservedAt)}
            </span>
            {/* The message, never the exception: adapters strip tokens and
                paths before the error ever reaches a committed health row. */}
            {failed && source.lastError !== null && (
              <span className="health-error">
                {source.lastError}
                {capability?.needsNetwork === true && source.state === "offline"
                  ? " · retrying with backoff"
                  : ""}
              </span>
            )}
          </li>
        );
      })}
    </ul>
  );
}
