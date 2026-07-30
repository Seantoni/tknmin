//! macOS menu bar indicator.
//!
//! A status item showing the token total at a glance, with the breakdown one
//! click away in its menu. It reads the same repository the window does, so
//! the two can never disagree.
//!
//! The window remains the full interface; this is a summary, not a second one.

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, Runtime};

use crate::domain::{Money, SummaryQuery, UsageQuota, UsageSummary};
use crate::state::AppState;

/// Install the status item. Called once during setup.
pub fn install<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    let summary = current_summary(app);
    let quotas = current_quotas(app);

    let menu = build_menu(app, summary.as_ref(), &quotas)?;

    TrayIconBuilder::with_id("usage")
        .title(title_for(summary.as_ref(), &quotas))
        // Left click opens the menu too, so the item behaves like every other
        // status item in the bar.
        .show_menu_on_left_click(true)
        .menu(&menu)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;

    Ok(())
}

fn current_summary<R: Runtime>(app: &AppHandle<R>) -> Option<UsageSummary> {
    let state = app.try_state::<AppState>()?;
    state.reader().summary(&SummaryQuery::default()).ok()
}

/// Every live allowance window, ordered source by source, shortest window
/// first — the order the menu reads in.
fn current_quotas<R: Runtime>(app: &AppHandle<R>) -> Vec<UsageQuota> {
    let Some(state) = app.try_state::<AppState>() else {
        return Vec::new();
    };
    let mut quotas = state.quotas();
    quotas.sort_by_key(|quota| (quota.source_app, quota.window_minutes));
    quotas
}

/// Repaint the status item after the store changed underneath it — called
/// after every import, or the bar would keep showing the startup summary.
pub fn refresh<R: Runtime>(app: &AppHandle<R>) {
    let Some(tray) = app.tray_by_id("usage") else {
        return;
    };
    let summary = current_summary(app);
    let quotas = current_quotas(app);
    let _ = tray.set_title(Some(&title_for(summary.as_ref(), &quotas)));
    if let Ok(menu) = build_menu(app, summary.as_ref(), &quotas) {
        let _ = tray.set_menu(Some(menu));
    }
}

/// What sits in the menu bar. Kept to a handful of characters — the bar is
/// shared with every other app on the machine. The token total comes first,
/// then one tagged allowance chip per source: `302K · claude 49% · codex 63%`.
///
/// A source can meter several windows at once (Claude runs a 5-hour session
/// inside a 7-day week). The bar shows only the one that binds first, because
/// that is the number that decides whether you can keep working; the menu
/// lists them all.
fn title_for(summary: Option<&UsageSummary>, quotas: &[UsageQuota]) -> String {
    let tokens = match summary {
        Some(summary) if summary.totals.record_count > 0 => {
            Some(compact_tokens(summary.totals.display_total.tokens))
        }
        _ => None,
    };

    let mut parts = Vec::new();
    if let Some(tokens) = tokens {
        parts.push(tokens);
    }
    for quota in tightest_per_source(quotas) {
        parts.push(format!(
            "{} {}%",
            source_tag(quota.source_app),
            round_tenths(quota.remaining_percent_tenths())
        ));
    }

    if parts.is_empty() {
        "—".to_string()
    } else {
        parts.join(" · ")
    }
}

/// The window with the least left over, once per source, in source order.
fn tightest_per_source(quotas: &[UsageQuota]) -> Vec<&UsageQuota> {
    let mut tightest: Vec<&UsageQuota> = Vec::new();
    for quota in quotas {
        match tightest
            .iter_mut()
            .find(|kept| kept.source_app == quota.source_app)
        {
            Some(kept) => {
                if quota.remaining_percent_tenths() < kept.remaining_percent_tenths() {
                    *kept = quota;
                }
            }
            None => tightest.push(quota),
        }
    }
    tightest
}

/// The shortest unambiguous name for the bar: `claude_code` reads as noise
/// next to `codex`, `claude` does not.
fn source_tag(source: crate::domain::SourceApp) -> &'static str {
    match source {
        crate::domain::SourceApp::Cursor => "cursor",
        crate::domain::SourceApp::ClaudeCode => "claude",
        crate::domain::SourceApp::Codex => "codex",
    }
}

/// Tenths of a percent to a whole percent, rounded half-up in integers.
fn round_tenths(tenths: u16) -> u16 {
    (tenths + 5) / 10
}

fn build_menu<R: Runtime>(
    app: &AppHandle<R>,
    summary: Option<&UsageSummary>,
    quotas: &[UsageQuota],
) -> tauri::Result<Menu<R>> {
    let menu = Menu::new(app)?;

    for quota in quotas {
        let meter = quota.label.as_ref().map_or_else(
            || source_tag(quota.source_app).to_string(),
            |label| format!("{} {}", source_tag(quota.source_app), label.to_lowercase()),
        );
        // A window with no reset has not started, and saying so is clearer than
        // a blank where every other line carries a time.
        let when = match quota.resets_at {
            Some(resets_at) => format!(
                "resets {}",
                resets_at
                    .with_timezone(&chrono::Local)
                    .format("%b %-d, %-I:%M%p")
            ),
            None => "not started".to_string(),
        };
        let line = format!(
            "{} · {}% of the {} left · {}",
            meter,
            round_tenths(quota.remaining_percent_tenths()),
            window_label(quota.window_minutes),
            when,
        );
        let id = format!("quota:{}", quota.source_app.as_str());
        menu.append(&disabled(app, &id, &line)?)?;
    }
    if !quotas.is_empty() {
        menu.append(&PredefinedMenuItem::separator(app)?)?;
    }

    if let Some(summary) = summary {
        let tokens = format!(
            "{} tokens",
            group_digits(summary.totals.display_total.tokens)
        );
        menu.append(&disabled(app, "total", &tokens)?)?;

        if let Some(cost) = summary.totals.cost.by_currency.first() {
            menu.append(&disabled(app, "cost", &format_money(&cost.amount))?)?;
        }

        if !summary.by_source.is_empty() {
            menu.append(&PredefinedMenuItem::separator(app)?)?;
            for group in &summary.by_source {
                let line = format!(
                    "{}  {}",
                    group.label,
                    group_digits(group.totals.display_total.tokens)
                );
                let id = format!("source:{}", group.key.clone().unwrap_or_default());
                menu.append(&disabled(app, &id, &line)?)?;
            }
        }

        menu.append(&PredefinedMenuItem::separator(app)?)?;
    }

    menu.append(&MenuItem::with_id(
        app,
        "open",
        "Open Tokens",
        true,
        None::<&str>,
    )?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(
        app,
        "quit",
        "Quit Tokens",
        true,
        None::<&str>,
    )?)?;

    Ok(menu)
}

/// An informational line: shown, never clickable.
fn disabled<R: Runtime>(app: &AppHandle<R>, id: &str, text: &str) -> tauri::Result<MenuItem<R>> {
    MenuItem::with_id(app, id, text, false, None::<&str>)
}

/// The allowance window in a word or two: "week", "session", "5h window".
fn window_label(window_minutes: u32) -> String {
    const WEEK: u32 = 10_080;
    const DAY: u32 = 1_440;
    match window_minutes {
        300 => "session".to_string(),
        WEEK => "week".to_string(),
        DAY => "day".to_string(),
        minutes if minutes > DAY && minutes % DAY == 0 => {
            format!("{}d period", minutes / DAY)
        }
        minutes if minutes % 60 == 0 => format!("{}h window", minutes / 60),
        minutes => format!("{minutes}min window"),
    }
}

fn show_window<R: Runtime>(app: &AppHandle<R>) {
    crate::mini::show_dashboard(app);
}

/// Menu-bar width is scarce, so large counts are abbreviated. One decimal is
/// enough to tell 1.2M from 1.9M, which is all this line is for.
fn compact_tokens(value: u64) -> String {
    const K: u64 = 1_000;
    const M: u64 = 1_000_000;
    const B: u64 = 1_000_000_000;

    let (scaled, suffix) = match value {
        v if v >= B => (scale_tenths(v, B), "B"),
        v if v >= M => (scale_tenths(v, M), "M"),
        v if v >= K => (scale_tenths(v, K), "K"),
        v => return v.to_string(),
    };

    let whole = scaled / 10;
    let tenth = scaled % 10;
    if tenth == 0 {
        format!("{whole}{suffix}")
    } else {
        format!("{whole}.{tenth}{suffix}")
    }
}

/// Value in tenths of `unit`, rounded half-up using integers only.
fn scale_tenths(value: u64, unit: u64) -> u64 {
    let divisor = unit / 10;
    let quotient = value / divisor;
    let remainder = value % divisor;
    if remainder * 2 >= divisor {
        quotient + 1
    } else {
        quotient
    }
}

/// Two decimal places, rounded in integer arithmetic. Money never becomes a
/// float, in the menu bar as anywhere else.
fn format_money(money: &Money) -> String {
    const PLACES: u32 = 2;

    let negative = money.amount_minor < 0;
    let magnitude = money.amount_minor.unsigned_abs();

    let scaled = if u32::from(money.minor_unit_exponent) > PLACES {
        let divisor = 10u64.pow(u32::from(money.minor_unit_exponent) - PLACES);
        let quotient = magnitude / divisor;
        let remainder = magnitude % divisor;
        if remainder * 2 >= divisor {
            quotient + 1
        } else {
            quotient
        }
    } else {
        magnitude * 10u64.pow(PLACES - u32::from(money.minor_unit_exponent))
    };

    let sign = if negative { "-" } else { "" };
    format!(
        "{sign}{}.{:02} {}",
        group_digits(scaled / 100),
        scaled % 100,
        money.currency
    )
}

/// Thousands separators, so a six-figure count stays readable in a menu.
fn group_digits(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(character);
    }
    grouped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abbreviates_only_where_it_helps() {
        assert_eq!(compact_tokens(0), "0");
        assert_eq!(compact_tokens(999), "999");
        assert_eq!(compact_tokens(1_000), "1K");
        assert_eq!(compact_tokens(302_819), "302.8K");
        assert_eq!(compact_tokens(1_240_000), "1.2M");
        // Half-up: exactly 1.25M rounds away from zero.
        assert_eq!(compact_tokens(1_250_000), "1.3M");
        assert_eq!(compact_tokens(4_000_000_000), "4B");
    }

    #[test]
    fn groups_digits() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1_000), "1,000");
        assert_eq!(group_digits(302_819), "302,819");
    }

    #[test]
    fn rounds_money_without_floats() {
        // 2.4888 USD held as micro-dollars rounds to 2.49.
        let micros = Money::new(2_488_800, "USD", 6).unwrap();
        assert_eq!(format_money(&micros), "2.49 USD");

        let cents = Money::new(125, "USD", 2).unwrap();
        assert_eq!(format_money(&cents), "1.25 USD");

        let whole = Money::new(7, "JPY", 0).unwrap();
        assert_eq!(format_money(&whole), "7.00 JPY");

        let negative = Money::new(-125, "USD", 2).unwrap();
        assert_eq!(format_money(&negative), "-1.25 USD");
    }

    #[test]
    fn title_falls_back_when_there_is_nothing_to_show() {
        assert_eq!(title_for(None, &[]), "—");
    }

    fn quota(
        source: crate::domain::SourceApp,
        window_minutes: u32,
        used_tenths: u16,
    ) -> UsageQuota {
        UsageQuota {
            source_app: source,
            label: None,
            window_minutes,
            used_percent_tenths: used_tenths,
            resets_at: Some(chrono::DateTime::from_timestamp(1_785_264_899, 0).unwrap()),
            observed_at: chrono::DateTime::from_timestamp(1_785_200_000, 0).unwrap(),
        }
    }

    #[test]
    fn title_shows_whole_percent_remaining() {
        let codex = |tenths| quota(crate::domain::SourceApp::Codex, 10_080, tenths);
        // 63.0% left.
        assert_eq!(title_for(None, &[codex(370)]), "codex 63%");
        // 62.5% left rounds half-up to 63%.
        assert_eq!(title_for(None, &[codex(375)]), "codex 63%");
        // A full window is 100%, never 1000‰.
        assert_eq!(title_for(None, &[codex(0)]), "codex 100%");
        // An exhausted window floors at zero.
        assert_eq!(title_for(None, &[codex(1_042)]), "codex 0%");
    }

    #[test]
    fn title_tags_each_sources_allowance() {
        let quotas = [
            quota(crate::domain::SourceApp::ClaudeCode, 300, 1_000),
            quota(crate::domain::SourceApp::Codex, 10_080, 370),
        ];
        assert_eq!(title_for(None, &quotas), "claude 0% · codex 63%");
    }

    #[test]
    fn title_shows_only_the_window_that_binds_first() {
        // Claude's session is 51% used and its week 38%, so the session is
        // what limits the next request: 49% left, not 62%.
        let quotas = [
            quota(crate::domain::SourceApp::ClaudeCode, 300, 510),
            quota(crate::domain::SourceApp::ClaudeCode, 10_080, 380),
            quota(crate::domain::SourceApp::Codex, 10_080, 370),
        ];
        assert_eq!(title_for(None, &quotas), "claude 49% · codex 63%");
        // Every window still reaches the menu, so nothing is lost.
        assert_eq!(tightest_per_source(&quotas).len(), 2);
    }

    #[test]
    fn window_labels_match_the_window() {
        assert_eq!(window_label(10_080), "week");
        assert_eq!(window_label(300), "session");
        assert_eq!(window_label(1_440), "day");
        assert_eq!(window_label(120), "2h window");
        assert_eq!(window_label(45), "45min window");
    }
}
