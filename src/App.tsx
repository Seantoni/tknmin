import { useState } from "react";

import { AlertBanner } from "./components/AlertBanner";
import { AppMenu } from "./components/AppMenu";
import { EventList } from "./components/EventList";
import { ModelBreakdown } from "./components/ModelBreakdown";
import { SettingsView } from "./components/SettingsView";
import { DashboardSkeleton, SettingsSkeleton } from "./components/Skeletons";
import { SourceHealthRow } from "./components/SourceHealth";
import { SourceStrip } from "./components/SourceStrip";
import { StatBar } from "./components/StatBar";
import { DASHBOARD_WINDOW_DAYS, useUsageDashboard } from "./hooks/useUsageDashboard";
import { useOptions } from "./hooks/useOptions";
import { useThresholdAlerts } from "./hooks/useThresholdAlerts";
import type { AppOptions } from "./domain/options";
import {
  describeQuotaWindow,
  describeQuotaWindows,
  formatPercentTenths,
  quotaRemainingTenths,
  tightestQuotaPerSource,
} from "./format";
import { SOURCE_LABEL } from "./theme";
import { showMiniWindow } from "./api/window";

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

  const onOptionsChange = (next: AppOptions) => {
    void updateOptions(next);
  };

  return (
    <div className="app">
      <header className="top">
        <div className="brand">
          <h1>tokens</h1>
          {screen === "dashboard" &&
            tightestQuotaPerSource(snapshot?.quotas ?? []).map((quota) => (
              <span
                key={quota.sourceApp}
                className="quota"
                title={describeQuotaWindows(
                  (snapshot?.quotas ?? []).filter((each) => each.sourceApp === quota.sourceApp),
                )}
              >
                {SOURCE_LABEL[quota.sourceApp]} ·{" "}
                {formatPercentTenths(quotaRemainingTenths(quota))} left{" "}
                {describeQuotaWindow(quota.windowMinutes)}
              </span>
            ))}
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
