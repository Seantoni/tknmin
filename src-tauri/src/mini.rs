//! The compact always-on-top window.
//!
//! A second, small, undecorated window showing one line per source: how much
//! of the binding allowance window is used, how much is left, and when it
//! resets. It exists so the numbers stay visible beside an editor without the
//! dashboard taking up the screen.
//!
//! It shows nothing the dashboard does not, and it reads the same state: both
//! windows subscribe to the same `data-changed` event and fetch the same
//! revisioned snapshot, so the two converge on one revision and cannot
//! disagree. Neither window decides when anything is read.
//!
//! Only one of the two windows is on screen at a time. The compact window is
//! built once during setup and then hidden and shown, rather than created and
//! destroyed, so toggling is instant and the webview keeps its subscriptions.

use tauri::{AppHandle, Manager, Runtime, WebviewUrl, WebviewWindowBuilder};

/// The dashboard. Declared in `tauri.conf.json`.
pub const MAIN_LABEL: &str = "main";
/// The compact window. The interface branches on this label, and the
/// capability in `capabilities/default.json` lists it.
pub const MINI_LABEL: &str = "mini";

/// Narrow enough to park in a corner. The height here only has to carry the
/// first paint: the interface measures its own content and resizes to it, since
/// how many rows there are depends on what the sources report.
const MINI_WIDTH: f64 = 300.0;
const MINI_HEIGHT: f64 = 200.0;

/// Build the compact window, hidden. Called once during setup, on the main
/// thread, where window creation belongs.
pub fn install<R: Runtime>(app: &AppHandle<R>, always_on_top: bool) -> tauri::Result<()> {
    WebviewWindowBuilder::new(app, MINI_LABEL, WebviewUrl::App("index.html".into()))
        .title("Tokens")
        .inner_size(MINI_WIDTH, MINI_HEIGHT)
        // Not resizable by hand — the content decides the height — but the
        // interface still sets the size itself as rows come and go.
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        // The window draws its own title row, so the system chrome would only
        // repeat it — and take a third of the height doing so.
        .decorations(false)
        // Rounded corners are the reason for transparency: the panel is drawn
        // in the document, and the window has to let its corners show through.
        .transparent(true)
        .always_on_top(always_on_top)
        // Without this the window stays behind on the Space it was opened in,
        // which defeats the point as soon as the editor is on another one.
        .visible_on_all_workspaces(true)
        .shadow(true)
        .theme(Some(tauri::Theme::Dark))
        .visible(false)
        .build()?;

    Ok(())
}

/// Show the compact window and put the dashboard away.
pub fn show_mini<R: Runtime>(app: &AppHandle<R>) {
    if let Some(mini) = app.get_webview_window(MINI_LABEL) {
        let _ = mini.show();
        let _ = mini.set_focus();
    }
    if let Some(main) = app.get_webview_window(MAIN_LABEL) {
        let _ = main.hide();
    }
}

/// Show the dashboard and put the compact window away. Also what the menu bar
/// item's "Open Tokens" does, so the two entry points behave alike.
pub fn show_dashboard<R: Runtime>(app: &AppHandle<R>) {
    if let Some(mini) = app.get_webview_window(MINI_LABEL) {
        let _ = mini.hide();
    }
    if let Some(main) = app.get_webview_window(MAIN_LABEL) {
        let _ = main.show();
        let _ = main.unminimize();
        let _ = main.set_focus();
    }
}

/// Apply the user's floating preference. Called when settings are saved, so the
/// checkbox takes effect on a window that is already open.
pub fn set_always_on_top<R: Runtime>(app: &AppHandle<R>, always_on_top: bool) {
    if let Some(mini) = app.get_webview_window(MINI_LABEL) {
        let _ = mini.set_always_on_top(always_on_top);
    }
}

pub fn is_mini_visible<R: Runtime>(app: &AppHandle<R>) -> bool {
    app.get_webview_window(MINI_LABEL)
        .and_then(|mini| mini.is_visible().ok())
        .unwrap_or(false)
}
