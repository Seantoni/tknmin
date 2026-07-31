//! macOS menu bar indicator.
//!
//! A status item showing what is left of each allowance at a glance, with the
//! token breakdown one click away in its menu. It reads the same repository
//! the window does, so the two can never disagree.
//!
//! The window remains the full interface; this is a summary, not a second one.

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, Runtime};
use chrono::{DateTime, Utc};

use crate::domain::{Money, PaceState, SummaryQuery, UsageQuota, UsageSummary, WindowPace};
use crate::state::AppState;

/// Install the status item. Called once during setup.
pub fn install<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    let summary = current_summary(app);
    let risk = current_risk(app);
    let quotas = sorted(risk.quotas);
    let paces = risk.pace;

    let menu = build_menu(app, summary.as_ref(), &quotas)?;

    TrayIconBuilder::with_id("usage")
        .title(title_for(&quotas, &paces, Utc::now()))
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

/// Allowances and their pace, in one read at one revision.
fn current_risk<R: Runtime>(app: &AppHandle<R>) -> crate::repository::RiskSnapshot {
    app.try_state::<AppState>()
        .map(|state| state.risk())
        .unwrap_or_else(|| crate::repository::RiskSnapshot {
            revision: 0,
            quotas: Vec::new(),
            health: Vec::new(),
            pace: Vec::new(),
            projections: Vec::new(),
        })
}

/// Source by source, shortest window first — the order the menu reads in.
fn sorted(mut quotas: Vec<UsageQuota>) -> Vec<UsageQuota> {
    quotas.sort_by_key(|quota| (quota.source_app, quota.window_minutes));
    quotas
}

/// Repaint the title after any committed revision — called even when no number
/// moved, because the title is not only numbers.
///
/// It carries countdowns: `claude 34%·2h`, `codex 31%↓14h·5d`. Those are
/// wrong the moment they stop counting, and a quiet stretch is exactly when
/// nothing else would have repainted them. Formatting a handful of quotas
/// costs nothing worth saving.
pub fn refresh_title<R: Runtime>(app: &AppHandle<R>) {
    let Some(tray) = app.tray_by_id("usage") else {
        return;
    };
    let risk = current_risk(app);
    let quotas = sorted(risk.quotas);
    let _ = tray.set_title(Some(&title_for(&quotas, &risk.pace, Utc::now())));
}

/// Repaint the whole item — title and menu — after the store changed
/// underneath it. The menu carries token totals, so this one does re-aggregate
/// and is reserved for revisions that actually moved data.
pub fn refresh<R: Runtime>(app: &AppHandle<R>) {
    let Some(tray) = app.tray_by_id("usage") else {
        return;
    };
    let summary = current_summary(app);
    let risk = current_risk(app);
    let quotas = sorted(risk.quotas);
    let _ = tray.set_title(Some(&title_for(&quotas, &risk.pace, Utc::now())));
    if let Ok(menu) = build_menu(app, summary.as_ref(), &quotas) {
        let _ = tray.set_menu(Some(menu));
    }
}

/// What sits in the menu bar. Kept to a handful of characters — the bar is
/// shared with every other app on the machine. One chip per source:
/// `claude 49%·2h · codex 31%↓14h·5d`. The percentage is what is left; the
/// trailing duration is when the window resets. A ↓ mark appears only when
/// the pace is heading for trouble, carrying how long the runway lasts so
/// "out in" can be read against the reset sitting next to it.
///
/// A source can meter several windows at once (Claude runs a 5-hour session
/// inside a 7-day week). The bar shows only the one that binds first, because
/// that is the number that decides whether you can keep working; the menu
/// lists them all.
fn title_for(quotas: &[UsageQuota], paces: &[WindowPace], now: DateTime<Utc>) -> String {
    let mut parts = Vec::new();
    for quota in binding_per_source(quotas, paces) {
        let percent = round_tenths(quota.remaining_percent_tenths());
        let mark = pace_mark(pace_for_quota(paces, quota));
        let until_reset = minutes_until_reset(quota, now)
            .map(|minutes| format!("·{}", compact_duration(minutes)))
            .unwrap_or_default();
        parts.push(format!(
            "{} {percent}%{mark}{until_reset}",
            source_tag(quota.source_app)
        ));
    }

    if parts.is_empty() {
        "—".to_string()
    } else {
        parts.join(" · ")
    }
}

/// The one window that decides whether a source can keep working — risk first,
/// then the least remaining — once per source, in the fixed reading order.
fn binding_per_source<'a>(quotas: &'a [UsageQuota], paces: &[WindowPace]) -> Vec<&'a UsageQuota> {
    let mut chosen: Vec<&UsageQuota> = Vec::new();
    for quota in quotas {
        match chosen
            .iter_mut()
            .find(|kept| kept.source_app == quota.source_app)
        {
            Some(kept) => {
                if binds_tighter(quota, *kept, paces) {
                    *kept = quota;
                }
            }
            None => chosen.push(quota),
        }
    }
    chosen.sort_by_key(|quota| source_order(quota.source_app));
    chosen
}

fn binds_tighter(candidate: &UsageQuota, held: &UsageQuota, paces: &[WindowPace]) -> bool {
    let candidate_pace = pace_for_quota(paces, candidate);
    let held_pace = pace_for_quota(paces, held);
    let candidate_priority = pace_priority(candidate_pace);
    let held_priority = pace_priority(held_pace);
    let candidate_runway = candidate_pace
        .and_then(|pace| pace.runway_minutes)
        .unwrap_or(u32::MAX);
    let held_runway = held_pace
        .and_then(|pace| pace.runway_minutes)
        .unwrap_or(u32::MAX);

    candidate_priority < held_priority
        || (candidate_priority == held_priority && candidate_runway < held_runway)
        || (candidate_priority == held_priority
            && candidate_runway == held_runway
            && candidate.remaining_percent_tenths() < held.remaining_percent_tenths())
}

fn pace_priority(pace: Option<&WindowPace>) -> u8 {
    match pace.map(|pace| pace.state) {
        Some(PaceState::Exhausted) => 0,
        Some(PaceState::Red) => 1,
        Some(PaceState::Amber) => 2,
        Some(PaceState::Unknown) => 3,
        Some(PaceState::Green) => 4,
        Some(PaceState::NotStarted) => 5,
        None => 6,
    }
}

fn pace_for_quota<'a>(paces: &'a [WindowPace], quota: &UsageQuota) -> Option<&'a WindowPace> {
    paces.iter().find(|pace| {
        pace.source_app == quota.source_app
            && pace.window_minutes == quota.window_minutes
            && pace.label == quota.label
    })
}

/// A mark only when the pace needs one. Quiet rows stay a percentage plus the
/// reset countdown; trouble gets a character the eye can catch at a glance,
/// and red also carries how long the runway lasts so "out in" is not left
/// hanging next to the reset.
fn pace_mark(pace: Option<&WindowPace>) -> String {
    let Some(pace) = pace else {
        return String::new();
    };
    match pace.state {
        PaceState::Red => match pace.runway_minutes {
            Some(minutes) => format!("↓{}", compact_duration(minutes)),
            None => "↓".to_string(),
        },
        PaceState::Amber => "~".to_string(),
        PaceState::Exhausted => "×".to_string(),
        PaceState::Green | PaceState::NotStarted | PaceState::Unknown => String::new(),
    }
}

/// Minutes until this window resets, when a reset is known and still ahead.
fn minutes_until_reset(quota: &UsageQuota, now: DateTime<Utc>) -> Option<u32> {
    let resets_at = quota.resets_at?;
    if resets_at <= now {
        return Some(0);
    }
    Some((resets_at - now).num_minutes().max(0) as u32)
}

/// Menu-bar width is scarce, so durations collapse to the coarsest unit that
/// still tells 2h from 2d.
fn compact_duration(minutes: u32) -> String {
    const HOUR: u32 = 60;
    const DAY: u32 = 1_440;
    if minutes < HOUR {
        format!("{minutes}m")
    } else if minutes < 2 * DAY {
        format!("{}h", (minutes + HOUR / 2) / HOUR)
    } else {
        format!("{}d", (minutes + DAY / 2) / DAY)
    }
}

fn source_order(source: crate::domain::SourceApp) -> u8 {
    match source {
        crate::domain::SourceApp::Codex => 0,
        crate::domain::SourceApp::ClaudeCode => 1,
        crate::domain::SourceApp::Cursor => 2,
    }
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
        // All three parts of the window's identity, because a source can meter
        // several at once: Claude's session and its week, Cursor's two pools.
        // Keyed on the source alone they collide, and two rows about one
        // source share an id that can only name one of them.
        menu.append(&disabled(app, &quota_item_id(quota), &line)?)?;
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

/// The menu id for one allowance window, unique across every window on screen.
fn quota_item_id(quota: &UsageQuota) -> String {
    format!(
        "quota:{}:{}:{}",
        quota.source_app.as_str(),
        quota.label.as_deref().unwrap_or_default(),
        quota.window_minutes
    )
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
    use crate::domain::{PaceVsBaseline, SourceApp};

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
        assert_eq!(title_for(&[], &[], now()), "—");
    }

    fn now() -> DateTime<Utc> {
        chrono::DateTime::from_timestamp(1_785_200_000, 0).unwrap()
    }

    fn quota(
        source: SourceApp,
        window_minutes: u32,
        used_tenths: u16,
        resets_in_minutes: i64,
    ) -> UsageQuota {
        UsageQuota {
            source_app: source,
            label: None,
            window_minutes,
            used_percent_tenths: used_tenths,
            resets_at: Some(now() + chrono::Duration::minutes(resets_in_minutes)),
            observed_at: now(),
        }
    }

    fn pace_of(
        source: SourceApp,
        window_minutes: u32,
        state: PaceState,
        runway_minutes: Option<u32>,
    ) -> WindowPace {
        WindowPace {
            source_app: source,
            label: None,
            window_minutes,
            state,
            runway_minutes,
            projected_exhaustion_at: None,
            shortfall_minutes: None,
            slack_minutes: None,
            pace_ratio_percent: None,
            basis: None,
            vs_baseline: PaceVsBaseline::NoBaseline,
            observed_at: now(),
            from_aged_reading: false,
        }
    }

    #[test]
    fn title_shows_whole_percent_remaining_and_reset_countdown() {
        let codex = |tenths| quota(SourceApp::Codex, 10_080, tenths, 5 * 1_440);
        // 63.0% left, week resets in 5 days.
        assert_eq!(title_for(&[codex(370)], &[], now()), "codex 63%·5d");
        // 62.5% left rounds half-up to 63%.
        assert_eq!(title_for(&[codex(375)], &[], now()), "codex 63%·5d");
        // A full window is 100%, never 1000‰.
        assert_eq!(title_for(&[codex(0)], &[], now()), "codex 100%·5d");
        // An exhausted window floors at zero.
        assert_eq!(title_for(&[codex(1_042)], &[], now()), "codex 0%·5d");
    }

    #[test]
    fn title_tags_each_sources_allowance_in_fixed_order() {
        let quotas = [
            quota(SourceApp::ClaudeCode, 300, 1_000, 120),
            quota(SourceApp::Codex, 10_080, 370, 5 * 1_440),
        ];
        assert_eq!(
            title_for(&quotas, &[], now()),
            "codex 63%·5d · claude 0%·2h"
        );
    }

    #[test]
    fn title_shows_only_the_window_that_binds_first() {
        // Claude's session is 51% used and its week 38%, so the session is
        // what limits the next request: 49% left, not 62%.
        let quotas = [
            quota(SourceApp::ClaudeCode, 300, 510, 120),
            quota(SourceApp::ClaudeCode, 10_080, 380, 5 * 1_440),
            quota(SourceApp::Codex, 10_080, 370, 5 * 1_440),
        ];
        assert_eq!(
            title_for(&quotas, &[], now()),
            "codex 63%·5d · claude 49%·2h"
        );
        assert_eq!(binding_per_source(&quotas, &[]).len(), 2);
    }

    #[test]
    fn title_marks_a_source_that_is_running_out_against_its_reset() {
        // Out in 2h, but the week still has 5 days — the gap is the whole point.
        let quotas = [quota(SourceApp::Codex, 10_080, 690, 5 * 1_440)];
        let paces = [pace_of(
            SourceApp::Codex,
            10_080,
            PaceState::Red,
            Some(120),
        )];
        assert_eq!(title_for(&quotas, &paces, now()), "codex 31%↓2h·5d");
    }

    #[test]
    fn title_marks_amber_and_still_shows_the_reset() {
        let quotas = [quota(SourceApp::ClaudeCode, 300, 500, 90)];
        let paces = [pace_of(SourceApp::ClaudeCode, 300, PaceState::Amber, Some(90))];
        assert_eq!(title_for(&quotas, &paces, now()), "claude 50%~·2h");
    }

    #[test]
    fn title_prefers_the_red_window_over_a_tighter_green_one() {
        // Week has less remaining, but the session is red — that is what the
        // bar must surface, or a green week would hide the session running out.
        let quotas = [
            quota(SourceApp::ClaudeCode, 300, 400, 90),
            quota(SourceApp::ClaudeCode, 10_080, 900, 5 * 1_440),
        ];
        let paces = [
            pace_of(SourceApp::ClaudeCode, 300, PaceState::Red, Some(45)),
            pace_of(SourceApp::ClaudeCode, 10_080, PaceState::Green, Some(5_000)),
        ];
        assert_eq!(title_for(&quotas, &paces, now()), "claude 60%↓45m·2h");
    }

    #[test]
    fn every_window_on_screen_gets_its_own_menu_id() {
        // Claude meters a session inside a week and Cursor meters two pools of
        // one cycle, so a source name alone names several rows at once.
        let mut cursor_other = quota(SourceApp::Cursor, 44_640, 375, 30 * 1_440);
        cursor_other.label = Some("Other Models".to_string());
        let mut cursor_own = quota(SourceApp::Cursor, 44_640, 34, 30 * 1_440);
        cursor_own.label = Some("Cursor Models".to_string());
        let quotas = [
            quota(SourceApp::ClaudeCode, 300, 510, 120),
            quota(SourceApp::ClaudeCode, 10_080, 380, 5 * 1_440),
            cursor_other,
            cursor_own,
        ];

        let mut ids: Vec<String> = quotas.iter().map(quota_item_id).collect();
        let held = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), held, "no two windows may share an id");
    }

    #[test]
    fn compact_durations_collapse_to_the_coarsest_useful_unit() {
        assert_eq!(compact_duration(12), "12m");
        assert_eq!(compact_duration(90), "2h");
        assert_eq!(compact_duration(1_440), "24h");
        assert_eq!(compact_duration(2_880), "2d");
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
