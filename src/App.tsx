import { useState } from "react";

import { AlertBanner } from "./components/AlertBanner";
import { AppMenu } from "./components/AppMenu";
import { EventList } from "./components/EventList";
import { ModelBreakdown } from "./components/ModelBreakdown";
import { SettingsView } from "./components/SettingsView";
import { DashboardSkeleton, SettingsSkeleton } from "./components/Skeletons";
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
import "./App.css";

type Screen = "dashboard" | "settings";

function App() {
  const [screen, setScreen] = useState<Screen>("dashboard");
  const [handoffCopiedKey, setHandoffCopiedKey] = useState<string | null>(null);
  const { status, data, error, isRefreshing, selectedSource, toggleSource, clearFilter, reload, refresh } =
    useUsageDashboard();
  const {
    status: optionsStatus,
    options,
    error: optionsError,
    isSaving,
    reload: reloadOptions,
    update: updateOptions,
  } = useOptions();
  const { alerts, continueAlert, createHandoff } = useThresholdAlerts();

  const onOptionsChange = (next: AppOptions) => {
    void updateOptions(next);
  };

  return (
    <div className="app">
      <header className="top">
        <div className="brand">
          <h1>tokens</h1>
          {screen === "dashboard" &&
            tightestQuotaPerSource(data?.quotas ?? []).map((quota) => (
              <span
                key={quota.sourceApp}
                className="quota"
                title={describeQuotaWindows(
                  (data?.quotas ?? []).filter((each) => each.sourceApp === quota.sourceApp),
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
                onClick={refresh}
                disabled={isRefreshing}
                title="rescan logs on disk and reload"
              >
                {isRefreshing ? "…" : "refresh"}
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
          onContinue={(key) => {
            setHandoffCopiedKey(null);
            void continueAlert(key);
          }}
          onCreateHandoff={(key) => {
            void createHandoff(key).then(() => {
              setHandoffCopiedKey(key);
            });
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

          {status === "ready" && data !== null && (
            <main>
              {data.recordCount === 0 ? (
                <p className="empty">No usage recorded yet.</p>
              ) : data.overview.totals.recordCount === 0 ? (
                <p className="empty">No usage in the last {DASHBOARD_WINDOW_DAYS} days.</p>
              ) : (
                <>
                  <StatBar totals={data.summary.totals} records={data.recent} />

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
                      overview={data.overview}
                      quotas={data.quotas}
                      selected={selectedSource}
                      onSelect={toggleSource}
                    />
                  </section>

                  <ModelBreakdown groups={data.summary.byModel} />

                  <section className="block">
                    <div className="block-head">
                      <span className="label">events</span>
                      <span className="count">
                        {data.summary.totals.recordCount}
                        {selectedSource !== null && ` of ${data.overview.totals.recordCount}`}
                      </span>
                    </div>
                    <EventList records={data.recent} />
                  </section>
                </>
              )}

              <footer className="foot">
                {data.sources
                  .map((source) => `${source.displayName} ${source.readsLogs ? "· live" : "· soon"}`)
                  .join("  ")}
                {data.summary.undatedRecordsExcluded > 0 &&
                  ` · ${data.summary.undatedRecordsExcluded} undated records excluded`}
              </footer>
            </main>
          )}
        </>
      )}
    </div>
  );
}

export default App;
