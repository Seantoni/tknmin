import { useEffect, useState } from "react";

import {
  connectCursorDashboard,
  disconnectCursorDashboard,
  fetchCursorConnection,
} from "../api/cursor";
import { testNotification } from "../api/notifications";
import type { AppOptions, SourceThreshold, ThresholdMetric } from "../domain/options";
import {
  clampThresholdValue,
  DEFAULT_THRESHOLD_VALUE,
  metricLabel,
  THRESHOLD_METRICS,
} from "../domain/options";
import type { SourceApp } from "../domain/usage";
import { SOURCE_COLOR, SOURCE_LABEL } from "../theme";

type SettingsViewProps = {
  options: AppOptions;
  isSaving: boolean;
  errorMessage: string | null;
  onBack: () => void;
  onChange: (options: AppOptions) => void;
};

const SOURCE_ORDER: SourceApp[] = ["claude_code", "cursor", "codex"];

function messageFromError(error: unknown): string {
  return typeof error === "object" && error !== null && "message" in error
    ? String((error as { message: unknown }).message)
    : String(error);
}

export function SettingsView({
  options,
  isSaving,
  errorMessage,
  onBack,
  onChange,
}: SettingsViewProps) {
  const [testingNotification, setTestingNotification] = useState(false);
  const [testMessage, setTestMessage] = useState<string | null>(null);
  const [cursorConnected, setCursorConnected] = useState<boolean | null>(null);
  const [cursorToken, setCursorToken] = useState("");
  const [cursorBusy, setCursorBusy] = useState(false);
  const [cursorMessage, setCursorMessage] = useState<string | null>(null);

  useEffect(() => {
    void fetchCursorConnection()
      .then((status) => setCursorConnected(status.connected))
      .catch((error) => setCursorMessage(messageFromError(error)));
  }, []);

  const rows = SOURCE_ORDER.map(
    (source) =>
      options.thresholds.find((row) => row.sourceApp === source) ?? {
        sourceApp: source,
        enabled: true,
        metric: "remaining_percent" as const,
        value: DEFAULT_THRESHOLD_VALUE.remaining_percent,
      },
  );

  const patch = (sourceApp: SourceApp, update: Partial<SourceThreshold>) => {
    const thresholds = SOURCE_ORDER.map((source) => {
      const current =
        options.thresholds.find((row) => row.sourceApp === source) ??
        rows.find((row) => row.sourceApp === source)!;
      if (source !== sourceApp) return current;
      const next = { ...current, ...update };
      if (update.metric !== undefined && update.value === undefined) {
        next.value = DEFAULT_THRESHOLD_VALUE[update.metric];
      }
      if (update.value !== undefined || update.metric !== undefined) {
        next.value = clampThresholdValue(next.metric, next.value);
      }
      return next;
    });
    onChange({ ...options, thresholds });
  };

  const onTestNotification = async () => {
    setTestingNotification(true);
    setTestMessage(null);
    try {
      await testNotification();
      setTestMessage("delivered — check Notification Center");
    } catch (error) {
      setTestMessage(messageFromError(error) || "could not deliver notification");
    } finally {
      setTestingNotification(false);
    }
  };

  const onConnectCursor = async () => {
    setCursorBusy(true);
    setCursorMessage("validating and syncing the last 30 days…");
    try {
      const status = await connectCursorDashboard(cursorToken);
      setCursorConnected(status.connected);
      setCursorToken("");
      setCursorMessage("connected · authoritative tokens and costs synced");
    } catch (error) {
      setCursorMessage(messageFromError(error));
    } finally {
      setCursorBusy(false);
    }
  };

  const onDisconnectCursor = async () => {
    setCursorBusy(true);
    setCursorMessage("disconnecting and restoring local data…");
    try {
      const status = await disconnectCursorDashboard();
      setCursorConnected(status.connected);
      setCursorMessage("disconnected · using incomplete local Cursor logs");
    } catch (error) {
      setCursorMessage(messageFromError(error));
    } finally {
      setCursorBusy(false);
    }
  };

  return (
    <main className="settings">
      <div className="settings-head">
        <button type="button" className="ghost small" onClick={onBack}>
          ← back
        </button>
        <h2>settings</h2>
        <span className="settings-status">{isSaving ? "saving…" : ""}</span>
      </div>

      <section className="block">
        <div className="block-head">
          <span className="label">alert thresholds</span>
        </div>
        <p className="settings-lead">
          Warn when a source’s allowance crosses the line you set — by percent left, or by time
          until reset.
        </p>

        <ul className="threshold-list">
          {rows.map((row) => (
            <li key={row.sourceApp} className="threshold-row">
              <div className="threshold-source">
                <span className="dot" style={{ background: SOURCE_COLOR[row.sourceApp] }} />
                <span className="strip-name">{SOURCE_LABEL[row.sourceApp]}</span>
                <label className="threshold-toggle">
                  <input
                    type="checkbox"
                    checked={row.enabled}
                    onChange={(event) => patch(row.sourceApp, { enabled: event.target.checked })}
                  />
                  <span>{row.enabled ? "on" : "off"}</span>
                </label>
              </div>

              <div className={`threshold-controls${row.enabled ? "" : " is-disabled"}`}>
                <select
                  className="threshold-select"
                  value={row.metric}
                  disabled={!row.enabled}
                  aria-label={`${SOURCE_LABEL[row.sourceApp]} threshold type`}
                  onChange={(event) =>
                    patch(row.sourceApp, { metric: event.target.value as ThresholdMetric })
                  }
                >
                  {THRESHOLD_METRICS.map((metric) => (
                    <option key={metric} value={metric}>
                      {metricLabel(metric)}
                    </option>
                  ))}
                </select>

                <label className="threshold-value">
                  <input
                    type="number"
                    className="threshold-input"
                    min={0}
                    max={row.metric === "remaining_percent" ? 100 : 10080}
                    step={1}
                    value={row.value}
                    disabled={!row.enabled}
                    aria-label={`${SOURCE_LABEL[row.sourceApp]} threshold value`}
                    onChange={(event) =>
                      patch(row.sourceApp, {
                        value: clampThresholdValue(row.metric, Number(event.target.value)),
                      })
                    }
                  />
                  <span className="threshold-unit">
                    {row.metric === "remaining_percent" ? "%" : "min"}
                  </span>
                </label>
              </div>
            </li>
          ))}
        </ul>
      </section>

      <section className="block">
        <div className="block-head">
          <span className="label">Cursor token usage</span>
          <span className={`connection-state${cursorConnected ? " is-connected" : ""}`}>
            {cursorConnected === null ? "checking…" : cursorConnected ? "connected" : "not connected"}
          </span>
        </div>
        <p className="settings-lead">
          Connect Cursor’s dashboard to import exact input, output, cache tokens, model, and billed
          cost. The credential is stored only in macOS Keychain.
        </p>
        {cursorConnected ? (
          <div className="settings-row">
            <button
              type="button"
              className="ghost"
              disabled={cursorBusy}
              onClick={() => void onDisconnectCursor()}
            >
              {cursorBusy ? "working…" : "disconnect"}
            </button>
            {cursorMessage !== null && <span className="settings-hint">{cursorMessage}</span>}
          </div>
        ) : (
          <>
            <p className="credential-help">
              In cursor.com/dashboard/usage, open browser DevTools → Application → Cookies and copy
              <code>WorkosCursorSessionToken</code>.
            </p>
            <div className="credential-row">
              <input
                type="password"
                className="credential-input"
                value={cursorToken}
                autoComplete="off"
                spellCheck={false}
                placeholder="paste session token"
                aria-label="Cursor dashboard session token"
                disabled={cursorBusy}
                onChange={(event) => setCursorToken(event.target.value)}
              />
              <button
                type="button"
                className="ghost"
                disabled={cursorBusy || cursorToken.trim().length === 0}
                onClick={() => void onConnectCursor()}
              >
                {cursorBusy ? "syncing…" : "connect"}
              </button>
            </div>
            {cursorMessage !== null && <p className="settings-hint credential-message">{cursorMessage}</p>}
          </>
        )}
      </section>

      <section className="block">
        <div className="block-head">
          <span className="label">compact window</span>
        </div>
        <p className="settings-lead">
          Minimize on the dashboard shrinks Tokens to a small window showing only what each source
          has used and has left. Keep it floating to read it beside a full-screen editor.
        </p>
        <div className="settings-row">
          <label className="threshold-toggle">
            <input
              type="checkbox"
              checked={options.miniAlwaysOnTop}
              onChange={(event) =>
                onChange({ ...options, miniAlwaysOnTop: event.target.checked })
              }
            />
            <span>stay on top of other windows</span>
          </label>
        </div>
      </section>

      <section className="block">
        <div className="block-head">
          <span className="label">notifications</span>
        </div>
        <p className="settings-lead">Preview the macOS alert that fires when a threshold is crossed.</p>
        <div className="settings-row">
          <button
            type="button"
            className="ghost"
            disabled={testingNotification}
            onClick={() => void onTestNotification()}
          >
            {testingNotification ? "sending…" : "test notification"}
          </button>
          {testMessage !== null && <span className="settings-hint">{testMessage}</span>}
        </div>
      </section>

      {errorMessage !== null && (
        <p className="settings-error" role="alert">
          {errorMessage}
        </p>
      )}
    </main>
  );
}
