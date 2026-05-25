#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use chrono::Local;
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::{
  env,
  fs,
  path::PathBuf,
  sync::atomic::{AtomicBool, Ordering},
};
use tauri::{
  AppHandle, CustomMenuItem, Manager, PhysicalPosition, State, SystemTray, SystemTrayEvent,
  SystemTrayMenu, Window, WindowEvent,
};

struct PinState(AtomicBool);

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UsageStats {
  db_path: String,
  total_tokens: i64,
  total_tokens_text: String,
  input_tokens: i64,
  output_tokens: i64,
  cache_tokens: i64,
  request_count: i64,
  total_cost_usd: f64,
  success_rate: f64,
}

#[derive(Debug, Deserialize, Serialize)]
struct WindowState {
  x: i32,
  y: i32,
}

fn state_path(app: &AppHandle) -> Option<PathBuf> {
  let dir = app.path_resolver().app_config_dir()?;
  let _ = fs::create_dir_all(&dir);
  Some(dir.join("window-state.json"))
}

fn read_window_state(app: &AppHandle) -> Option<WindowState> {
  let path = state_path(app)?;
  let content = fs::read_to_string(path).ok()?;
  serde_json::from_str(&content).ok()
}

fn save_window_state(app: &AppHandle, position: PhysicalPosition<i32>) {
  let Some(path) = state_path(app) else {
    return;
  };
  let state = WindowState {
    x: position.x,
    y: position.y,
  };
  if let Ok(content) = serde_json::to_string_pretty(&state) {
    let _ = fs::write(path, content);
  }
}

fn find_db() -> PathBuf {
  if let Ok(explicit) = env::var("CC_SWITCH_DB") {
    return PathBuf::from(explicit);
  }

  let mut candidates = Vec::new();
  if let Ok(user_profile) = env::var("USERPROFILE") {
    candidates.push(PathBuf::from(user_profile).join(".cc-switch").join("cc-switch.db"));
  }
  if let Ok(home) = env::var("HOME") {
    candidates.push(PathBuf::from(home).join(".cc-switch").join("cc-switch.db"));
  }
  if let Some(home) = dirs::home_dir() {
    candidates.push(home.join(".cc-switch").join("cc-switch.db"));
  }

  candidates
    .iter()
    .find(|candidate| candidate.exists())
    .cloned()
    .unwrap_or_else(|| {
      candidates
        .into_iter()
        .next()
        .unwrap_or_else(|| PathBuf::from(".cc-switch").join("cc-switch.db"))
    })
}

fn format_tokens(value: i64) -> String {
  if value >= 1_000_000 {
    format!("{:.2}M", value as f64 / 1_000_000.0)
  } else if value >= 10_000 {
    format!("{:.1}K", value as f64 / 1_000.0)
  } else {
    value.to_string()
  }
}

fn period_bounds(period: &str) -> Result<(i64, i64), String> {
  let now = Local::now();
  let end_ts = now.timestamp();

  let days = match period {
    "7d" => 7,
    "30d" => 30,
    _ => 1, // today
  };

  let start = now - chrono::Duration::days(days);
  let start_ts = start.timestamp();

  Ok((start_ts, end_ts))
}

#[tauri::command]
fn get_stats(period: String) -> Result<UsageStats, String> {
  let db_path = find_db();
  if !db_path.exists() {
    return Err(format!("cc-switch db not found: {}", db_path.display()));
  }

  let (start_ts, end_ts) = period_bounds(&period)?;
  let conn = Connection::open_with_flags(
    &db_path,
    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
  )
  .map_err(|error| error.to_string())?;

  let mut stmt = conn
    .prepare(
      r#"
      SELECT
          COUNT(*) AS total_requests,
          COALESCE(SUM(CAST(l.total_cost_usd AS REAL)), 0) AS total_cost,
          COALESCE(SUM(
              CASE
                  WHEN l.app_type IN ('codex', 'gemini') AND l.input_tokens >= l.cache_read_tokens
                  THEN l.input_tokens - l.cache_read_tokens
                  ELSE l.input_tokens
              END
          ), 0) AS fresh_input_tokens,
          COALESCE(SUM(l.output_tokens), 0) AS output_tokens,
          COALESCE(SUM(l.cache_creation_tokens), 0) AS cache_creation_tokens,
          COALESCE(SUM(l.cache_read_tokens), 0) AS cache_read_tokens,
          COALESCE(SUM(CASE WHEN l.status_code >= 200 AND l.status_code < 300 THEN 1 ELSE 0 END), 0) AS success_count
      FROM proxy_request_logs l
      WHERE l.created_at BETWEEN ?1 AND ?2
        AND NOT (
          COALESCE(l.data_source, 'proxy') IN ('session_log', 'codex_session', 'gemini_session')
          AND EXISTS (
              SELECT 1
              FROM proxy_request_logs proxy_dedup
              WHERE COALESCE(proxy_dedup.data_source, 'proxy') = 'proxy'
                AND proxy_dedup.app_type = l.app_type
                AND proxy_dedup.status_code >= 200
                AND proxy_dedup.status_code < 300
                AND proxy_dedup.input_tokens = l.input_tokens
                AND proxy_dedup.output_tokens = l.output_tokens
                AND proxy_dedup.cache_read_tokens = l.cache_read_tokens
                AND (
                  proxy_dedup.cache_creation_tokens = l.cache_creation_tokens
                  OR (
                    l.cache_creation_tokens = 0
                    AND COALESCE(l.data_source, 'proxy') IN ('codex_session', 'gemini_session')
                  )
                )
                AND proxy_dedup.created_at BETWEEN l.created_at - 600 AND l.created_at + 600
                AND (
                  LOWER(proxy_dedup.model) = LOWER(l.model)
                  OR LOWER(proxy_dedup.model) = 'unknown'
                  OR LOWER(l.model) = 'unknown'
                )
          )
        )
      "#,
    )
    .map_err(|error| error.to_string())?;

  let (request_count, total_cost_usd, input_tokens, output_tokens, cache_creation, cache_read, success_count): (
    i64,
    f64,
    i64,
    i64,
    i64,
    i64,
    i64,
  ) = stmt
    .query_row(params![start_ts, end_ts], |row| {
      Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
      ))
    })
    .map_err(|error| error.to_string())?;

  let cache_tokens = cache_creation + cache_read;
  let total_tokens = input_tokens + output_tokens + cache_tokens;
  let success_rate = if request_count > 0 {
    success_count as f64 / request_count as f64 * 100.0
  } else {
    0.0
  };

  Ok(UsageStats {
    db_path: db_path.display().to_string(),
    total_tokens,
    total_tokens_text: format_tokens(total_tokens),
    input_tokens,
    output_tokens,
    cache_tokens,
    request_count,
    total_cost_usd,
    success_rate,
  })
}

#[tauri::command]
fn hide_window(window: Window) -> Result<(), String> {
  window.hide().map_err(|error| error.to_string())
}

#[tauri::command]
fn start_dragging(window: Window) -> Result<(), String> {
  window.start_dragging().map_err(|error| error.to_string())
}

#[tauri::command]
fn toggle_top(window: Window, state: State<'_, PinState>) -> Result<bool, String> {
  let next = !state.0.load(Ordering::Relaxed);
  window
    .set_always_on_top(next)
    .map_err(|error| error.to_string())?;
  state.0.store(next, Ordering::Relaxed);
  Ok(next)
}

fn main() {
  let show = CustomMenuItem::new("show".to_string(), "Show / Hide");
  let refresh = CustomMenuItem::new("refresh".to_string(), "Refresh");
  let quit = CustomMenuItem::new("quit".to_string(), "Quit");
  let tray = SystemTray::new().with_menu(
    SystemTrayMenu::new()
      .add_item(show)
      .add_item(refresh)
      .add_item(quit),
  );

  tauri::Builder::default()
    .manage(PinState(AtomicBool::new(true)))
    .system_tray(tray)
    .invoke_handler(tauri::generate_handler![
      get_stats,
      hide_window,
      start_dragging,
      toggle_top
    ])
    .setup(|app| {
      let window = app
        .get_window("main")
        .ok_or_else(|| "missing main window".to_string())?;
      if let Some(state) = read_window_state(&app.handle()) {
        let _ = window.set_position(PhysicalPosition::new(state.x, state.y));
      }
      window.show()?;
      Ok(())
    })
    .on_window_event(|event| {
      if let WindowEvent::Moved(position) = event.event() {
        save_window_state(&event.window().app_handle(), *position);
      }
    })
    .on_system_tray_event(|app, event| match event {
      SystemTrayEvent::LeftClick { .. } => {
        if let Some(window) = app.get_window("main") {
          if window.is_visible().unwrap_or(false) {
            let _ = window.hide();
          } else {
            let _ = window.show();
            let _ = window.set_focus();
          }
        }
      }
      SystemTrayEvent::MenuItemClick { id, .. } => match id.as_str() {
        "show" => {
          if let Some(window) = app.get_window("main") {
            if window.is_visible().unwrap_or(false) {
              let _ = window.hide();
            } else {
              let _ = window.show();
              let _ = window.set_focus();
            }
          }
        }
        "refresh" => {
          if let Some(window) = app.get_window("main") {
            let _ = window.eval("refreshStats()");
          }
        }
        "quit" => app.exit(0),
        _ => {}
      },
      _ => {}
    })
    .run(tauri::generate_context!())
    .expect("error while running Token Pet");
}
