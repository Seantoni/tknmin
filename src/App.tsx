import { useState } from "react";

import { AlertBanner } from "./components/AlertBanner";
import { AppMenu } from "./components/AppMenu";
import { EventList } from "./components/EventList";
import { ModelBreakdown } from "./components/ModelBreakdown";
import { QuotaRows } from "./components/QuotaRows";
import { SettingsView } from "./components/SettingsView";
import { DashboardSkeleton, SettingsSkeleton } from "./components/Skeletons";
import { SourceHealthRow } from "./components/SourceHealth";
import { SourceStrip } from "./components/SourceStrip";
import { StatBar } from "./components/StatBar";
import { ThreadUsage } from "./components/ThreadUsage";
import { DASHBOARD_WINDOW_DAYS, useUsageDashboard } from "./hooks/useUsageDashboard";
import { useOptions } from "./hooks/useOptions";
import { useThresholdAlerts } from "./hooks/useThresholdAlerts";
import type { AppOptions } from "./domain/options";
import {
  describePaceCompact,
  describePaceGap,
  describeQuotaWindow,
  describeQuotaWindows,
  formatPercentTenths,
  paceForQuota,
  quotaRemainingTenths,
} from "./format";
import { SOURCE_LABEL } from "./theme";
import { showMiniWindow } from "./api/window";
import { useNow } from "./hooks/useNow";
import type { WindowPace } from "./domain/pace";
import type { SourceApp, UsageQuota } from "./domain/usage";

function pacePriority(pace: WindowPace | undefined): number {
  if (pace === undefined) return 6;
  switch (pace.state) {
    case "exhausted":
      return 0;
    case "red":
      return 1;
    case "amber":
      return 2;
    case "unknown":
      return 3;
    case "green":
      return 4;
    case "notStarted":
      return 5;
  }
}

/**
 * One quota per source for the header, chosen by projected risk first.
 *
 * Remaining percentage alone can hide the binding window: a green week with
 * 10% left must not suppress a red session with 40% left.
 */
function headerQuotaPerSource(quotas: UsageQuota[], paces: WindowPace[]): UsageQuota[] {
  const chosen = new Map<SourceApp, UsageQuota>();
  for (const candidate of quotas) {
    const held = chosen.get(candidate.sourceApp);
    if (held === undefined) {
      chosen.set(candidate.sourceApp, candidate);
      continue;
    }

    const candidatePace = paceForQuota(paces, candidate);
    const heldPace = paceForQuota(paces, held);
    const candidatePriority = pacePriority(candidatePace);
    const heldPriority = pacePriority(heldPace);
    const candidateRunway = candidatePace?.runwayMinutes ?? Number.MAX_SAFE_INTEGER;
    const heldRunway = heldPace?.runwayMinutes ?? Number.MAX_SAFE_INTEGER;

    if (
      candidatePriority < heldPriority ||
      (candidatePriority === heldPriority && candidateRunway < heldRunway) ||
      (candidatePriority === heldPriority &&
        candidateRunway === heldRunway &&
        quotaRemainingTenths(candidate) < quotaRemainingTenths(held))
    ) {
      chosen.set(candidate.sourceApp, candidate);
    }
  }
  return [...chosen.values()];
}

type Screen = "dashboard" | "settings";

function App() {
  const [screen, setScreen] = useState<Screen>("dashboard");
  const {
    status,
    snapshot,
    sources,
    error,
    isRefreshing,
    selectedSource,
    toggleSource,
    clearFilter,
    reload,
    requestSync,
  } = useUsageDashboard();
  const {
    status: optionsStatus,
    options,
    error: optionsError,
    isSaving,
    reload: reloadOptions,
    update: updateOptions,
  } = useOptions();
  const { alerts, handoffCopiedKey, continueAlert, createHandoff } = useThresholdAlerts();
  const now = useNow();

  const onOptionsChange = (next: AppOptions) => {
    void updateOptions(next);
  };

  return (
    <div className="app">
      <header className="top">
        <div className="brand">
          <h1>tokens</h1>
          {screen === "dashboard" &&
            headerQuotaPerSource(snapshot?.quotas ?? [], snapshot?.pace ?? []).map((quota) => {
              const pace = paceForQuota(snapshot?.pace ?? [], quota);
              const verdict = pace === undefined ? null : describePaceCompact(pace);
              const gap = pace === undefined ? null : describePaceGap(pace);
              return (
                <span
                  key={quota.sourceApp}
                  className={`quota${pace !== undefined && (pace.state === "red" || pace.state === "amber" || pace.state === "green") ? ` is-${pace.state}` : ""}`}
                  title={describeQuotaWindows(
                    (snapshot?.quotas ?? []).filter((each) => each.sourceApp === quota.sourceApp),
                  )}
                >
                  {SOURCE_LABEL[quota.sourceApp]} ·{" "}
                  <span className={pace?.state === "red" ? "quota-left is-red" : undefined}>
                    {formatPercentTenths(quotaRemainingTenths(quota))} left
                  </span>{" "}
                  {describeQuotaWindow(quota.windowMinutes)}
                  {verdict !== null && (
                    <span className="quota-pace">
                      {" "}
                      · {verdict}
                      {gap !== null && ` · ${gap}`}
                    </span>
                  )}
                </span>
              );
            })}
        </div>
        <div className="top-right">
          {screen === "dashboard" && (
            <>
              <span className="window-note">last {DASHBOARD_WINDOW_DAYS} days</span>
              <button
                type="button"
                className="ghost"
                onClick={requestSync}
                disabled={isRefreshing}
                title="sources update on their own; this only asks the app to catch up now"
              >
                {isRefreshing ? "…" : "sync now"}
              </button>
              <button
                type="button"
                className="ghost"
                onClick={() => void showMiniWindow()}
                title="shrink to a small window showing only what is used and left"
              >
                minimize
              </button>
            </>
          )}
          <AppMenu onOpenSettings={() => setScreen("settings")} />
        </div>
      </header>

      <div
        className={`progress${status === "loading" || isRefreshing || optionsStatus === "loading" || isSaving ? " is-active" : ""}`}
      />

      {screen === "dashboard" && (
        <AlertBanner
          alerts={alerts}
          handoffCopiedKey={handoffCopiedKey}
          onContinue={(key) => void continueAlert(key)}
          onCreateHandoff={(key) => {
            void createHandoff(key);
          }}
        />
      )}

      {screen === "settings" ? (
        optionsStatus === "loading" ? (
          <SettingsSkeleton />
        ) : optionsStatus === "error" && optionsError !== null ? (
          <div className="failure" role="alert">
            <p>{optionsError.message}</p>
            <button type="button" className="ghost" onClick={() => void reloadOptions()}>
              try again
            </button>
          </div>
        ) : (
          <SettingsView
            options={options}
            isSaving={isSaving}
            errorMessage={optionsError?.message ?? null}
            onBack={() => setScreen("dashboard")}
            onChange={onOptionsChange}
          />
        )
      ) : (
        <>
          {status === "loading" && <DashboardSkeleton />}

          {status === "error" && error !== null && (
            <div className="failure" role="alert">
              <p>{error.message}</p>
              <button type="button" className="ghost" onClick={reload}>
                try again
              </button>
            </div>
          )}

          {status === "ready" && snapshot !== null && (
            <main>
              {snapshot.recordCount === 0 ? (
                <p className="empty">No usage recorded yet.</p>
              ) : snapshot.overview.totals.recordCount === 0 ? (
                <p className="empty">No usage in the last {DASHBOARD_WINDOW_DAYS} days.</p>
              ) : (
                <>
                  <StatBar totals={snapshot.summary.totals} records={snapshot.recent} />

                  {snapshot.quotas.length > 0 && (
                    // Above the token breakdowns on purpose: those say what has
                    // been spent, this says whether there is enough left to keep
                    // working, which is the question a person opens this for.
                    <section className="block allowances">
                      <div className="block-head">
                        <span className="label">allowances</span>
                      </div>
                      <QuotaRows
                        quotas={snapshot.quotas}
                        pace={snapshot.pace}
                        projections={snapshot.projections}
                        health={snapshot.health}
                        now={now}
                      />
                    </section>
                  )}

                  <section className="block">
                    <div className="block-head">
                      <span className="label">sources</span>
                      {selectedSource !== null && (
                        <button type="button" className="ghost small" onClick={clearFilter}>
                          showing {SOURCE_LABEL[selectedSource]} · clear
                        </button>
                      )}
                    </div>
                    <SourceStrip
                      overview={snapshot.overview}
                      quotas={snapshot.quotas}
                      selected={selectedSource}
                      onSelect={toggleSource}
                    />
                  </section>

                  <ModelBreakdown groups={snapshot.summary.byModel} />

                  <ThreadUsage report={snapshot.threadUsage} />

                  <section className="block">
                    <div className="block-head">
                      <span className="label">events</span>
                      <span className="count">
                        {snapshot.summary.totals.recordCount}
                        {selectedSource !== null && ` of ${snapshot.overview.totals.recordCount}`}
                      </span>
                    </div>
                    <EventList records={snapshot.recent} />
                  </section>
                </>
              )}

              <SourceHealthRow health={snapshot.health} sources={sources} />

              <footer className="foot">
                {snapshot.summary.undatedRecordsExcluded > 0 &&
                  `${snapshot.summary.undatedRecordsExcluded} undated records excluded`}
              </footer>
            </main>
          )}
        </>
      )}
    </div>
  );
}

export default App;
