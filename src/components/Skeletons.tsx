/**
 * Placeholder outlines shown while a screen's first load runs.
 *
 * The interface deliberately avoids spinners (the one-pixel progress line
 * signals activity), so a cold load instead gets the shape of what's coming —
 * stat columns, strip rows, event rows — drawn in the same hairlines and dims
 * as the real layout, pulsing gently until data replaces them.
 */

import type { CSSProperties } from "react";

/** One placeholder block; width varies, height matches the text it stands in for. */
function Bone({
  width,
  height,
  className = "",
}: {
  width: number | string;
  height?: number;
  className?: string;
}) {
  return <span className={`skeleton ${className}`} style={{ width, height }} />;
}

/** Stagger each row's pulse so the screen ripples rather than blinks in unison. */
const stagger = (index: number) => ({ "--d": `${index * 90}ms` }) as CSSProperties;

const STAT_LABELS = ["tokens", "cost", "events"];

export function DashboardSkeleton() {
  return (
    <main role="status" aria-label="loading usage data">
      <div aria-hidden="true">
        <div className="stat-bar">
          {STAT_LABELS.map((label, index) => (
            <div className="stat" key={label} style={stagger(index)}>
              <div className="stat-label">{label}</div>
              <div style={{ marginTop: 5 }}>
                <Bone width={64} height={20} />
              </div>
              <div style={{ marginTop: 8 }}>
                <Bone width={92} />
              </div>
            </div>
          ))}
        </div>

        <section className="block">
          <div className="block-head">
            <span className="label">sources</span>
          </div>
          <ul className="strip">
            {[0, 1, 2].map((row) => (
              <li key={row} style={stagger(row + 3)}>
                <div className="strip-row">
                  <Bone width={6} height={6} className="skeleton-dot" />
                  <Bone width={72} />
                  <span className="bar" />
                  <Bone width={26} className="skeleton-right" />
                  <Bone width={44} className="skeleton-right" />
                  <Bone width={32} className="skeleton-right" />
                </div>
              </li>
            ))}
          </ul>
        </section>

        <section className="block">
          <div className="block-head">
            <span className="label">events</span>
          </div>
          <ul className="events">
            {[0, 1, 2, 3, 4].map((row) => (
              <li key={row} style={stagger(row + 6)}>
                <div className="event-row is-static">
                  <span />
                  <Bone width={48} className="event-time" />
                  <Bone width={60} className="event-source" />
                  <Bone width="62%" />
                  <Bone width={40} className="skeleton-right" />
                  <Bone width={30} className="skeleton-right event-cost" />
                </div>
              </li>
            ))}
          </ul>
        </section>
      </div>
    </main>
  );
}

export function SettingsSkeleton() {
  return (
    <main className="settings" role="status" aria-label="loading settings">
      <div aria-hidden="true">
        <div className="settings-head">
          <h2>settings</h2>
        </div>

        <section className="block">
          <div className="block-head">
            <span className="label">alert thresholds</span>
          </div>
          <ul className="threshold-list">
            {[0, 1, 2].map((row) => (
              <li key={row} className="threshold-row" style={stagger(row)}>
                <div className="threshold-source">
                  <Bone width={6} height={6} className="skeleton-dot" />
                  <Bone width={72} />
                  <Bone width={38} className="skeleton-push" />
                </div>
                <div className="threshold-controls">
                  <Bone width={168} height={29} />
                  <Bone width={72} height={29} />
                </div>
              </li>
            ))}
          </ul>
        </section>
      </div>
    </main>
  );
}
