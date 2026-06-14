#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use chrono::{DateTime, Local};
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashSet},
    env, fs,
    io::{BufRead, BufReader},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use tauri::{
    AppHandle, CustomMenuItem, LogicalSize, Manager, PhysicalPosition, PhysicalSize, State,
    SystemTray, SystemTrayEvent, SystemTrayMenu, Window, WindowEvent,
};
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::HWND as WinHwnd,
    UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, DestroyMenu, SetForegroundWindow, TrackPopupMenu, MF_CHECKED,
        MF_SEPARATOR, MF_STRING, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    },
};

struct PinState(AtomicBool);
struct DataSourceState(Mutex<DataSource>);
struct EdgeRuntimeState(Mutex<EdgeRuntime>);
struct LocalUsageCacheState(Mutex<LocalUsageCache>);

const EXPANDED_SIZE: (f64, f64) = (192.0, 284.0);
const COMPACT_SIZE: (f64, f64) = (192.0, 78.0);
const EDGE_DOCK_SIZE: (f64, f64) = (49.0, 134.0);
const EDGE_SNAP_DISTANCE: i32 = 12;
const EDGE_CURSOR_SNAP_DISTANCE: i32 = 8;
const SCREEN_PADDING: i32 = 8;
const EDGE_MOVE_SUPPRESS_MS: u64 = 900;
const EDGE_MOVE_SETTLE_MS: u64 = 180;
const LOCAL_USAGE_CACHE_TTL: Duration = Duration::from_secs(30);
#[cfg(windows)]
const MENU_SHOW_HIDE: u32 = 1;
#[cfg(windows)]
const MENU_REFRESH: u32 = 2;
#[cfg(windows)]
const MENU_SOURCE_LOCAL: u32 = 3;
#[cfg(windows)]
const MENU_SOURCE_CCSWITCH: u32 = 4;
#[cfg(windows)]
const MENU_QUIT: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DataSource {
    LocalLogs,
    CcSwitch,
}

impl DataSource {
    fn as_str(self) -> &'static str {
        match self {
            DataSource::LocalLogs => "local",
            DataSource::CcSwitch => "ccswitch",
        }
    }
}

#[derive(Default)]
struct UsageTotals {
    input_tokens: i64,
    output_tokens: i64,
    cache_tokens: i64,
    request_count: i64,
    total_cost_usd: f64,
    success_count: i64,
}

#[derive(Debug, Clone, Serialize)]
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

#[derive(Clone)]
struct UsageRecord {
    ts: i64,
    model: Option<String>,
    input_tokens: i64,
    output_tokens: i64,
    cache_tokens: i64,
}

#[derive(Clone)]
struct LocalUsageSnapshot {
    source_path: String,
    records: Vec<UsageRecord>,
    models: Vec<String>,
}

#[derive(Default)]
struct LocalUsageCache {
    loaded_at: Option<Instant>,
    snapshot: Option<LocalUsageSnapshot>,
}

#[derive(Debug, Deserialize, Serialize)]
struct WindowState {
    x: i32,
    y: i32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct EdgeDockState {
    docked: bool,
    edge: Option<String>,
}

struct EdgeRuntime {
    suppress_until: Option<Instant>,
    move_generation: u64,
    drag_cursor_offset_x: Option<i32>,
    drag_cursor_offset_y: Option<i32>,
    last_cursor_x: Option<i32>,
    drag_compact: bool,
    current_edge: Option<String>,
    drag_active: bool,
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

fn refresh_window(app: &AppHandle) {
    if let Some(window) = app.get_window("main") {
        let _ = window.eval("window.refreshDashboard?.()");
    }
}

fn update_source_menu(app: &AppHandle, source: DataSource) {
    let tray = app.tray_handle();
    let _ = tray
        .get_item("source_local")
        .set_title(if source == DataSource::LocalLogs {
            "[x] Local Codex/Claude"
        } else {
            "[ ] Local Codex/Claude"
        });
    let _ = tray
        .get_item("source_ccswitch")
        .set_title(if source == DataSource::CcSwitch {
            "[x] cc-switch"
        } else {
            "[ ] cc-switch"
        });
}

fn set_data_source(app: &AppHandle, next: DataSource) {
    let state = app.state::<DataSourceState>();
    if let Ok(mut source) = state.0.lock() {
        *source = next;
    }
    update_source_menu(app, next);
    refresh_window(app);
    if let Some(menu_win) = app.get_window("context_menu") {
        let src_str = match next {
            DataSource::LocalLogs => "local",
            DataSource::CcSwitch => "ccswitch",
        };
        let _ = menu_win.eval(&format!("window.updateMarkers(\"{src_str}\")"));
    }
}

#[cfg(windows)]
fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
unsafe fn append_native_menu_item(menu: isize, id: u32, label: &str, checked: bool) {
    let label = wide_null(label);
    let checked_flag = if checked { MF_CHECKED } else { 0 };
    AppendMenuW(menu, MF_STRING | checked_flag, id as usize, label.as_ptr());
}

fn find_db() -> PathBuf {
    if let Ok(explicit) = env::var("CC_SWITCH_DB") {
        return PathBuf::from(explicit);
    }

    let mut candidates = Vec::new();
    if let Ok(user_profile) = env::var("USERPROFILE") {
        candidates.push(
            PathBuf::from(user_profile)
                .join(".cc-switch")
                .join("cc-switch.db"),
        );
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
    if value >= 1_000_000_000 {
        format!("{:.2}B", value as f64 / 1_000_000_000.0)
    } else if value >= 1_000_000 {
        format!("{:.2}M", value as f64 / 1_000_000.0)
    } else if value >= 10_000 {
        format!("{:.1}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn period_bounds(period: &str) -> Result<(Option<i64>, i64), String> {
    let now = Local::now();
    let end_ts = now.timestamp();

    let days = match period {
        "all" => return Ok((None, end_ts)),
        "today" => 0,
        "7d" => 7,
        "30d" => 30,
        _ => return Err(format!("unsupported period: {period}")),
    };

    let start = if days == 0 {
        // today: start from today 00:00
        now.date_naive().and_hms_opt(0, 0, 0).unwrap()
    } else {
        // 7d/30d: go back by days, then to 00:00
        (now.date_naive() - chrono::Duration::days(days))
            .and_hms_opt(0, 0, 0)
            .unwrap()
    };

    let start_ts = start
        .and_local_timezone(Local)
        .single()
        .ok_or_else(|| "failed to resolve local start time".to_string())?
        .timestamp();

    Ok((Some(start_ts), end_ts))
}

fn normalize_model_filter(model: Option<String>) -> Option<String> {
    model
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty() && value != "all")
}

fn model_matches(actual: Option<&str>, filter: Option<&str>) -> bool {
    let Some(filter) = filter else {
        return true;
    };

    actual.is_some_and(|model| model.eq_ignore_ascii_case(filter))
}

fn stats_from_totals(source_path: String, totals: UsageTotals) -> UsageStats {
    let total_tokens = totals.input_tokens + totals.output_tokens + totals.cache_tokens;
    let success_rate = if totals.request_count > 0 {
        totals.success_count as f64 / totals.request_count as f64 * 100.0
    } else {
        0.0
    };

    UsageStats {
        db_path: source_path,
        total_tokens,
        total_tokens_text: format_tokens(total_tokens),
        input_tokens: totals.input_tokens,
        output_tokens: totals.output_tokens,
        cache_tokens: totals.cache_tokens,
        request_count: totals.request_count,
        total_cost_usd: totals.total_cost_usd,
        success_rate,
    }
}

fn get_ccswitch_stats(period: &str, model: Option<String>) -> Result<UsageStats, String> {
    let db_path = find_db();
    if !db_path.exists() {
        return Err(format!("cc-switch db not found: {}", db_path.display()));
    }

    let (start_ts, end_ts) = period_bounds(&period)?;
    let model_filter = normalize_model_filter(model);
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
      WHERE (?1 IS NULL OR l.created_at BETWEEN ?1 AND ?2)
        AND (?3 IS NULL OR LOWER(l.model) = LOWER(?3))
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

    let (
        request_count,
        total_cost_usd,
        input_tokens,
        output_tokens,
        cache_creation,
        cache_read,
        success_count,
    ): (i64, f64, i64, i64, i64, i64, i64) = stmt
        .query_row(params![start_ts, end_ts, model_filter.as_deref()], |row| {
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

    Ok(stats_from_totals(
        db_path.display().to_string(),
        UsageTotals {
            input_tokens,
            output_tokens,
            cache_tokens: cache_creation + cache_read,
            request_count,
            total_cost_usd,
            success_count,
        },
    ))
}

fn get_ccswitch_models(period: &str) -> Result<Vec<String>, String> {
    let db_path = find_db();
    if !db_path.exists() {
        return Ok(Vec::new());
    }

    let (start_ts, end_ts) = period_bounds(period)?;
    let conn = Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|error| error.to_string())?;

    let mut stmt = conn
        .prepare(
            r#"
            SELECT DISTINCT TRIM(model)
            FROM proxy_request_logs
            WHERE (?1 IS NULL OR created_at BETWEEN ?1 AND ?2)
              AND model IS NOT NULL
              AND TRIM(model) != ''
            ORDER BY LOWER(TRIM(model))
            "#,
        )
        .map_err(|error| error.to_string())?;

    let rows = stmt
        .query_map(params![start_ts, end_ts], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?;
    let mut models = Vec::new();
    for row in rows {
        if let Ok(model) = row {
            models.push(model);
        }
    }
    Ok(models)
}

fn collect_jsonl_files(root: PathBuf, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl_files(path, files);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("jsonl") {
            files.push(path);
        }
    }
}

fn parse_timestamp(value: &Value) -> Option<i64> {
    let timestamp = value.get("timestamp")?.as_str()?;
    DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|dt| dt.timestamp())
}

fn int_field(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn string_field<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    for key in keys {
        if let Some(value) = value.get(*key).and_then(Value::as_str) {
            if !value.trim().is_empty() {
                return Some(value.trim());
            }
        }
    }
    None
}

fn codex_model(value: &Value) -> Option<&str> {
    let payload = &value["payload"];
    string_field(&payload["info"], &["model", "model_slug"])
        .or_else(|| string_field(payload, &["model"]))
        .or_else(|| string_field(value, &["model"]))
}

fn claude_model(value: &Value) -> Option<&str> {
    string_field(&value["message"], &["model"]).or_else(|| string_field(value, &["model"]))
}

fn local_log_files() -> Result<(Vec<PathBuf>, Vec<PathBuf>), String> {
    let home = dirs::home_dir().ok_or_else(|| "home directory not found".to_string())?;

    let mut codex_files = Vec::new();
    collect_jsonl_files(home.join(".codex").join("sessions"), &mut codex_files);

    let mut claude_files = Vec::new();
    collect_jsonl_files(home.join(".claude").join("projects"), &mut claude_files);

    Ok((codex_files, claude_files))
}

fn add_codex_usage_record(
    value: &Value,
    records: &mut Vec<UsageRecord>,
    seen: &mut HashSet<String>,
    file_model: Option<&str>,
) {
    let Some(ts) = parse_timestamp(value) else {
        return;
    };

    let payload = &value["payload"];
    if payload.get("type").and_then(Value::as_str) != Some("token_count") {
        return;
    }

    let usage = &payload["info"]["last_token_usage"];
    if !usage.is_object() {
        return;
    }

    let input_tokens = int_field(usage, "input_tokens");
    let cached_input_tokens = int_field(usage, "cached_input_tokens");
    let output_tokens =
        int_field(usage, "output_tokens") + int_field(usage, "reasoning_output_tokens");
    let key = format!("{ts}:{input_tokens}:{cached_input_tokens}:{output_tokens}");
    if !seen.insert(key) {
        return;
    }

    records.push(UsageRecord {
        ts,
        model: codex_model(value).or(file_model).map(ToString::to_string),
        input_tokens: (input_tokens - cached_input_tokens).max(0),
        output_tokens,
        cache_tokens: cached_input_tokens,
    });
}

fn add_claude_usage_record(
    value: &Value,
    records: &mut Vec<UsageRecord>,
    seen: &mut HashSet<String>,
) {
    let Some(ts) = parse_timestamp(value) else {
        return;
    };

    let message = &value["message"];
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return;
    }

    let usage = &message["usage"];
    if !usage.is_object() {
        return;
    }

    let id = message
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| value.get("uuid").and_then(Value::as_str))
        .unwrap_or("");
    let input_tokens = int_field(usage, "input_tokens");
    let cache_creation_total = int_field(usage, "cache_creation_input_tokens");
    let cache_creation_breakdown = usage
        .get("cache_creation")
        .and_then(|cache| cache.get("ephemeral_1h_input_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0)
        + usage
            .get("cache_creation")
            .and_then(|cache| cache.get("ephemeral_5m_input_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
    let cache_creation_tokens = if cache_creation_total > 0 {
        cache_creation_total
    } else {
        cache_creation_breakdown
    };
    let cache_read_tokens = int_field(usage, "cache_read_input_tokens");
    let output_tokens = int_field(usage, "output_tokens");
    if input_tokens + cache_creation_tokens + cache_read_tokens + output_tokens == 0 {
        return;
    }

    let key = if id.is_empty() {
        format!("{ts}:{input_tokens}:{cache_creation_tokens}:{cache_read_tokens}:{output_tokens}")
    } else {
        id.to_string()
    };
    if !seen.insert(key) {
        return;
    }

    records.push(UsageRecord {
        ts,
        model: claude_model(value).map(ToString::to_string),
        input_tokens,
        output_tokens,
        cache_tokens: cache_creation_tokens + cache_read_tokens,
    });
}

fn collect_local_usage_snapshot() -> Result<LocalUsageSnapshot, String> {
    let (codex_files, claude_files) = local_log_files()?;
    let mut records = Vec::new();
    let mut codex_seen = HashSet::new();
    let mut claude_seen = HashSet::new();
    let mut models = BTreeSet::new();

    for path in &codex_files {
        let Ok(file) = fs::File::open(path) else {
            continue;
        };
        let reader = BufReader::new(file);
        let mut file_model: Option<String> = None;
        for line in reader.lines().flatten() {
            if !line.contains("usage") && !line.contains("token_count") && !line.contains("model") {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                if let Some(model) = codex_model(&value) {
                    file_model = Some(model.to_string());
                    models.insert(model.to_string());
                }
                add_codex_usage_record(
                    &value,
                    &mut records,
                    &mut codex_seen,
                    file_model.as_deref(),
                );
            }
        }
    }

    for path in &claude_files {
        let Ok(file) = fs::File::open(path) else {
            continue;
        };
        let reader = BufReader::new(file);
        for line in reader.lines().flatten() {
            if !line.contains("usage") {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                if let Some(model) = claude_model(&value) {
                    models.insert(model.to_string());
                }
                add_claude_usage_record(&value, &mut records, &mut claude_seen);
            }
        }
    }

    Ok(LocalUsageSnapshot {
        source_path: format!(
            "local logs: {} Codex files, {} Claude files",
            codex_files.len(),
            claude_files.len()
        ),
        records,
        models: models.into_iter().collect(),
    })
}

fn cached_local_usage(cache: &LocalUsageCacheState) -> Result<LocalUsageSnapshot, String> {
    let mut cache = cache
        .0
        .lock()
        .map_err(|_| "failed to lock local usage cache".to_string())?;

    if let (Some(loaded_at), Some(snapshot)) = (cache.loaded_at, cache.snapshot.as_ref()) {
        if loaded_at.elapsed() < LOCAL_USAGE_CACHE_TTL {
            return Ok(snapshot.clone());
        }
    }

    let snapshot = collect_local_usage_snapshot()?;
    cache.loaded_at = Some(Instant::now());
    cache.snapshot = Some(snapshot.clone());
    Ok(snapshot)
}

fn totals_from_records(
    records: &[UsageRecord],
    start_ts: Option<i64>,
    end_ts: i64,
    model: Option<String>,
) -> UsageTotals {
    let model_filter = normalize_model_filter(model);
    let mut totals = UsageTotals::default();

    for record in records {
        if start_ts.is_some_and(|start_ts| record.ts < start_ts) || record.ts > end_ts {
            continue;
        }
        if !model_matches(record.model.as_deref(), model_filter.as_deref()) {
            continue;
        }

        totals.input_tokens += record.input_tokens;
        totals.output_tokens += record.output_tokens;
        totals.cache_tokens += record.cache_tokens;
        totals.request_count += 1;
        totals.success_count += 1;
    }

    totals
}

fn get_local_log_stats(
    period: &str,
    model: Option<String>,
    cache: &LocalUsageCacheState,
) -> Result<UsageStats, String> {
    let (start_ts, end_ts) = period_bounds(period)?;
    let snapshot = cached_local_usage(cache)?;
    let totals = totals_from_records(&snapshot.records, start_ts, end_ts, model);
    Ok(stats_from_totals(snapshot.source_path, totals))
}

fn get_local_log_models(period: &str, cache: &LocalUsageCacheState) -> Result<Vec<String>, String> {
    let (start_ts, end_ts) = period_bounds(period)?;
    let snapshot = cached_local_usage(cache)?;
    let mut models = BTreeSet::new();

    for record in &snapshot.records {
        if start_ts.is_some_and(|start_ts| record.ts < start_ts) || record.ts > end_ts {
            continue;
        }
        if let Some(model) = &record.model {
            models.insert(model.clone());
        }
    }

    if models.is_empty() {
        Ok(snapshot.models)
    } else {
        Ok(models.into_iter().collect())
    }
}

fn window_logical_size(compact: bool) -> LogicalSize<f64> {
    if compact {
        LogicalSize::new(COMPACT_SIZE.0, COMPACT_SIZE.1)
    } else {
        LogicalSize::new(EXPANDED_SIZE.0, EXPANDED_SIZE.1)
    }
}

fn clamp_to_monitor(value: i32, min: i32, max: i32) -> i32 {
    value.max(min).min(max)
}

fn suppress_edge_settle_state(runtime: &EdgeRuntimeState) {
    if let Ok(mut state) = runtime.0.lock() {
        state.suppress_until = Some(Instant::now() + Duration::from_millis(EDGE_MOVE_SUPPRESS_MS));
    }
}

fn suppress_edge_settle(runtime: &State<'_, EdgeRuntimeState>) {
    suppress_edge_settle_state(runtime);
}

fn is_edge_settle_suppressed(runtime: &EdgeRuntimeState) -> bool {
    runtime
        .0
        .lock()
        .ok()
        .and_then(|state| state.suppress_until)
        .is_some_and(|until| Instant::now() < until)
}

fn next_move_generation(runtime: &EdgeRuntimeState) -> u64 {
    if let Ok(mut state) = runtime.0.lock() {
        state.move_generation += 1;
        state.move_generation
    } else {
        0
    }
}

fn current_move_generation(runtime: &EdgeRuntimeState) -> u64 {
    runtime
        .0
        .lock()
        .map(|state| state.move_generation)
        .unwrap_or(0)
}

fn drag_cursor_offset_x(runtime: &EdgeRuntimeState) -> Option<i32> {
    runtime
        .0
        .lock()
        .ok()
        .and_then(|state| state.drag_cursor_offset_x)
}

fn last_cursor_x(runtime: &EdgeRuntimeState) -> Option<i32> {
    runtime.0.lock().ok().and_then(|state| state.last_cursor_x)
}

fn edge_runtime_snapshot(runtime: &EdgeRuntimeState) -> (Option<String>, bool) {
    runtime
        .0
        .lock()
        .map(|state| (state.current_edge.clone(), state.drag_compact))
        .unwrap_or((None, false))
}

fn is_drag_active(runtime: &EdgeRuntimeState) -> bool {
    runtime
        .0
        .lock()
        .map(|state| state.drag_active)
        .unwrap_or(false)
}

fn set_current_edge(runtime: &EdgeRuntimeState, edge: Option<String>) {
    if let Ok(mut state) = runtime.0.lock() {
        state.current_edge = edge;
    }
}

fn current_monitor_bounds(window: &Window) -> Result<(PhysicalPosition<i32>, PhysicalSize<u32>), String> {
    let monitor = window
        .current_monitor()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "failed to resolve current monitor".to_string())?;
    Ok((*monitor.position(), *monitor.size()))
}

fn keep_window_in_monitor(window: &Window, runtime: &EdgeRuntimeState) {
    let (current_edge, _) = edge_runtime_snapshot(runtime);
    if current_edge.is_some() {
        return;
    }

    let Ok((monitor_position, monitor_size)) = current_monitor_bounds(window) else {
        return;
    };
    let Ok(position) = window.outer_position() else {
        return;
    };
    let Ok(size) = window.outer_size() else {
        return;
    };

    let monitor_left = monitor_position.x;
    let monitor_top = monitor_position.y;
    let monitor_right = monitor_left + monitor_size.width as i32;
    let monitor_bottom = monitor_top + monitor_size.height as i32;
    let min_x = monitor_left + SCREEN_PADDING;
    let max_x = monitor_right - size.width as i32 - SCREEN_PADDING;
    let min_y = monitor_top + SCREEN_PADDING;
    let max_y = monitor_bottom - size.height as i32 - SCREEN_PADDING;
    let target_x = clamp_to_monitor(position.x, min_x, max_x);
    let target_y = clamp_to_monitor(position.y, min_y, max_y);

    if target_x != position.x || target_y != position.y {
        suppress_edge_settle_state(runtime);
        let _ = window.set_position(PhysicalPosition::new(target_x, target_y));
    }
}

fn set_edge_dock(window: &Window, edge: &str) -> Result<EdgeDockState, String> {
    let (monitor_position, monitor_size) = current_monitor_bounds(window)?;
    let position = window.outer_position().map_err(|error| error.to_string())?;
    let current_size = window.outer_size().map_err(|error| error.to_string())?;
    let dock_size = LogicalSize::new(EDGE_DOCK_SIZE.0, EDGE_DOCK_SIZE.1);
    let dock_physical_size = dock_size.to_physical::<u32>(window.scale_factor().unwrap_or(1.0));

    let monitor_left = monitor_position.x;
    let monitor_top = monitor_position.y;
    let monitor_right = monitor_left + monitor_size.width as i32;
    let monitor_bottom = monitor_top + monitor_size.height as i32;
    let current_center_y = position.y + current_size.height as i32 / 2;
    let target_y = clamp_to_monitor(
        current_center_y - dock_physical_size.height as i32 / 2,
        monitor_top + SCREEN_PADDING,
        monitor_bottom - dock_physical_size.height as i32 - SCREEN_PADDING,
    );
    let target_x = if edge == "left" {
        monitor_left
    } else {
        monitor_right - dock_physical_size.width as i32
    };

    window.set_size(dock_size).map_err(|error| error.to_string())?;
    window
        .set_position(PhysicalPosition::new(target_x, target_y))
        .map_err(|error| error.to_string())?;

    Ok(EdgeDockState {
        docked: true,
        edge: Some(edge.to_string()),
    })
}

fn expand_from_edge(window: &Window, edge: &str, compact: bool) -> Result<(), String> {
    let (monitor_position, monitor_size) = current_monitor_bounds(window)?;
    let position = window.outer_position().map_err(|error| error.to_string())?;
    let current_size = window.outer_size().map_err(|error| error.to_string())?;
    let target_size = window_logical_size(compact);
    let target_physical_size = target_size.to_physical::<u32>(window.scale_factor().unwrap_or(1.0));

    let monitor_left = monitor_position.x;
    let monitor_top = monitor_position.y;
    let monitor_right = monitor_left + monitor_size.width as i32;
    let monitor_bottom = monitor_top + monitor_size.height as i32;
    let current_center_y = position.y + current_size.height as i32 / 2;
    let target_y = clamp_to_monitor(
        current_center_y - target_physical_size.height as i32 / 2,
        monitor_top + SCREEN_PADDING,
        monitor_bottom - target_physical_size.height as i32 - SCREEN_PADDING,
    );
    let target_x = if edge == "left" {
        monitor_left
    } else {
        monitor_right - target_physical_size.width as i32
    };

    window.set_size(target_size).map_err(|error| error.to_string())?;
    window
        .set_position(PhysicalPosition::new(target_x, target_y))
        .map_err(|error| error.to_string())
}

fn expand_from_drag_position(
    window: &Window,
    edge: &str,
    compact: bool,
    runtime: &EdgeRuntimeState,
) -> Result<(), String> {
    let (monitor_position, monitor_size) = current_monitor_bounds(window)?;
    let position = window.outer_position().map_err(|error| error.to_string())?;
    let current_size = window.outer_size().map_err(|error| error.to_string())?;
    let scale_factor = window.scale_factor().map_err(|error| error.to_string())?;
    let target_size = window_logical_size(compact);
    let target_physical_size = target_size.to_physical::<u32>(scale_factor);
    let dock_physical_width = LogicalSize::new(EDGE_DOCK_SIZE.0, EDGE_DOCK_SIZE.1)
        .to_physical::<u32>(scale_factor)
        .width as i32;
    let target_width = target_physical_size.width as i32;
    let target_height = target_physical_size.height as i32;
    let dock_offset = drag_cursor_offset_x(runtime).unwrap_or(current_size.width as i32 / 2);
    let cursor_x = position.x + dock_offset;
    let target_offset = if edge == "right" {
        target_width - (dock_physical_width - dock_offset)
    } else {
        dock_offset
    }
    .max(0)
    .min(target_width);

    let monitor_left = monitor_position.x;
    let monitor_top = monitor_position.y;
    let monitor_right = monitor_left + monitor_size.width as i32;
    let monitor_bottom = monitor_top + monitor_size.height as i32;
    let current_center_y = position.y + current_size.height as i32 / 2;
    let target_x = clamp_to_monitor(
        cursor_x - target_offset,
        monitor_left + SCREEN_PADDING,
        monitor_right - target_width - SCREEN_PADDING,
    );
    let target_y = clamp_to_monitor(
        current_center_y - target_height / 2,
        monitor_top + SCREEN_PADDING,
        monitor_bottom - target_height - SCREEN_PADDING,
    );

    window.set_size(target_size).map_err(|error| error.to_string())?;
    window
        .set_position(PhysicalPosition::new(target_x, target_y))
        .map_err(|error| error.to_string())
}

fn edge_from_cursor_or_window(
    window: &Window,
    runtime: Option<&EdgeRuntimeState>,
) -> Result<Option<&'static str>, String> {
    let (monitor_position, monitor_size) = current_monitor_bounds(window)?;
    let position = window.outer_position().map_err(|error| error.to_string())?;
    let size = window.outer_size().map_err(|error| error.to_string())?;

    let monitor_left = monitor_position.x;
    let monitor_right = monitor_left + monitor_size.width as i32;

    if let Some(cursor_x) = runtime.and_then(last_cursor_x).or_else(|| {
        runtime
            .and_then(drag_cursor_offset_x)
            .map(|cursor_offset_x| position.x + cursor_offset_x)
    }) {
        let cursor_left_distance = (cursor_x - monitor_left).abs();
        let cursor_right_distance = (monitor_right - cursor_x).abs();

        if cursor_left_distance <= EDGE_CURSOR_SNAP_DISTANCE
            && cursor_left_distance <= cursor_right_distance
        {
            return Ok(Some("left"));
        }

        if cursor_right_distance <= EDGE_CURSOR_SNAP_DISTANCE {
            return Ok(Some("right"));
        }
    }

    let left_distance = (position.x - monitor_left).abs();
    let right_distance = (monitor_right - (position.x + size.width as i32)).abs();

    if left_distance <= EDGE_SNAP_DISTANCE && left_distance <= right_distance {
        return Ok(Some("left"));
    }

    if right_distance <= EDGE_SNAP_DISTANCE {
        return Ok(Some("right"));
    }

    Ok(None)
}

#[tauri::command]
fn get_stats(
    period: String,
    model: Option<String>,
    source: State<'_, DataSourceState>,
    local_cache: State<'_, LocalUsageCacheState>,
) -> Result<UsageStats, String> {
    let data_source = *source
        .0
        .lock()
        .map_err(|_| "failed to lock data source".to_string())?;

    match data_source {
        DataSource::LocalLogs => get_local_log_stats(&period, model, &local_cache),
        DataSource::CcSwitch => get_ccswitch_stats(&period, model),
    }
}

#[tauri::command]
fn get_models(
    period: String,
    source: State<'_, DataSourceState>,
    local_cache: State<'_, LocalUsageCacheState>,
) -> Result<Vec<String>, String> {
    let data_source = *source
        .0
        .lock()
        .map_err(|_| "failed to lock data source".to_string())?;

    match data_source {
        DataSource::LocalLogs => get_local_log_models(&period, &local_cache),
        DataSource::CcSwitch => get_ccswitch_models(&period),
    }
}

#[tauri::command]
fn get_data_source(source: State<'_, DataSourceState>) -> Result<String, String> {
    let data_source = *source
        .0
        .lock()
        .map_err(|_| "failed to lock data source".to_string())?;
    Ok(data_source.as_str().to_string())
}

#[tauri::command]
fn set_data_source_from_window(app: AppHandle, source: String) -> Result<String, String> {
    let next = match source.as_str() {
        "local" => DataSource::LocalLogs,
        "ccswitch" => DataSource::CcSwitch,
        _ => return Err(format!("unsupported data source: {source}")),
    };

    set_data_source(&app, next);
    Ok(next.as_str().to_string())
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

#[cfg(windows)]
#[tauri::command]
fn show_native_context_menu(
    app: AppHandle,
    window: Window,
    client_x: f64,
    client_y: f64,
    source: State<'_, DataSourceState>,
) -> Result<(), String> {
    let current_source = *source
        .0
        .lock()
        .map_err(|_| "failed to lock data source".to_string())?;
    let hwnd: WinHwnd = window.hwnd().map_err(|error| error.to_string())?.0;
    let window_position = window.outer_position().map_err(|error| error.to_string())?;
    let scale_factor = window.scale_factor().map_err(|error| error.to_string())?;
    let x = window_position.x + (client_x * scale_factor).round() as i32;
    let y = window_position.y + (client_y * scale_factor).round() as i32;
    let menu = unsafe { CreatePopupMenu() };
    if menu == 0 {
        return Err("failed to create native context menu".to_string());
    }

    unsafe {
        append_native_menu_item(menu, MENU_SHOW_HIDE, "Show / Hide", false);
        append_native_menu_item(menu, MENU_REFRESH, "Refresh", false);
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        append_native_menu_item(
            menu,
            MENU_SOURCE_LOCAL,
            "Local Codex/Claude",
            current_source == DataSource::LocalLogs,
        );
        append_native_menu_item(
            menu,
            MENU_SOURCE_CCSWITCH,
            "cc-switch",
            current_source == DataSource::CcSwitch,
        );
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        append_native_menu_item(menu, MENU_QUIT, "Quit", false);

        SetForegroundWindow(hwnd);
        let command = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            x,
            y,
            0,
            hwnd,
            std::ptr::null(),
        ) as u32;
        DestroyMenu(menu);

        match command {
            MENU_SHOW_HIDE => {
                let _ = window.hide();
            }
            MENU_REFRESH => refresh_window(&app),
            MENU_SOURCE_LOCAL => set_data_source(&app, DataSource::LocalLogs),
            MENU_SOURCE_CCSWITCH => set_data_source(&app, DataSource::CcSwitch),
            MENU_QUIT => app.exit(0),
            _ => {}
        }
    }

    Ok(())
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
fn begin_edge_drag(
    window: Window,
    cursor_offset_x: f64,
    cursor_offset_y: f64,
    screen_x: f64,
    compact: bool,
    docked_edge: Option<String>,
    runtime: State<'_, EdgeRuntimeState>,
) -> Result<(), String> {
    let scale_factor = window.scale_factor().map_err(|error| error.to_string())?;
    let offset_x = (cursor_offset_x * scale_factor).round() as i32;
    let offset_y = (cursor_offset_y * scale_factor).round() as i32;
    let cursor_x = (screen_x * scale_factor).round() as i32;
    if let Ok(mut state) = runtime.0.lock() {
        state.drag_cursor_offset_x = Some(offset_x.max(0));
        state.drag_cursor_offset_y = Some(offset_y.max(0));
        state.last_cursor_x = Some(cursor_x);
        state.drag_compact = compact;
        state.drag_active = true;
        if docked_edge.is_some() {
            state.current_edge = docked_edge;
        }
    }
    Ok(())
}

#[tauri::command]
fn drag_move_window(
    window: Window,
    screen_x: f64,
    screen_y: f64,
    cursor_offset_x: f64,
    cursor_offset_y: f64,
    runtime: State<'_, EdgeRuntimeState>,
) -> Result<(), String> {
    let scale_factor = window.scale_factor().map_err(|error| error.to_string())?;
    let cursor_x = (screen_x * scale_factor).round() as i32;
    let cursor_y = (screen_y * scale_factor).round() as i32;
    let offset_x = (cursor_offset_x * scale_factor).round() as i32;
    let offset_y = (cursor_offset_y * scale_factor).round() as i32;
    if let Ok(mut state) = runtime.0.lock() {
        state.drag_cursor_offset_x = Some(offset_x.max(0));
        state.drag_cursor_offset_y = Some(offset_y.max(0));
        state.last_cursor_x = Some(cursor_x);
        state.drag_active = true;
    }

    let (monitor_position, monitor_size) = current_monitor_bounds(&window)?;
    let size = window.outer_size().map_err(|error| error.to_string())?;
    let monitor_left = monitor_position.x;
    let monitor_top = monitor_position.y;
    let monitor_right = monitor_left + monitor_size.width as i32;
    let monitor_bottom = monitor_top + monitor_size.height as i32;
    let target_x = clamp_to_monitor(
        cursor_x - offset_x,
        monitor_left + SCREEN_PADDING,
        monitor_right - size.width as i32 - SCREEN_PADDING,
    );
    let target_y = clamp_to_monitor(
        cursor_y - offset_y,
        monitor_top + SCREEN_PADDING,
        monitor_bottom - size.height as i32 - SCREEN_PADDING,
    );

    window
        .set_position(PhysicalPosition::new(target_x, target_y))
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn undock_and_start_drag(
    window: Window,
    edge: String,
    compact: bool,
    cursor_offset_x: f64,
    runtime: State<'_, EdgeRuntimeState>,
) -> Result<(), String> {
    suppress_edge_settle(&runtime);

    let scale_factor = window.scale_factor().map_err(|error| error.to_string())?;
    let dock_width = LogicalSize::new(EDGE_DOCK_SIZE.0, EDGE_DOCK_SIZE.1)
        .to_physical::<u32>(scale_factor)
        .width as i32;
    let target_width = window_logical_size(compact)
        .to_physical::<u32>(scale_factor)
        .width as i32;
    let dock_offset = (cursor_offset_x * scale_factor).round() as i32;
    let offset = if edge == "right" {
        target_width - (dock_width - dock_offset)
    } else {
        dock_offset
    }
    .max(0)
    .min(target_width);
    if let Ok(mut state) = runtime.0.lock() {
        state.drag_cursor_offset_x = Some(offset);
        state.drag_compact = compact;
        state.current_edge = None;
    }

    expand_from_edge(&window, &edge, compact)?;
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

#[tauri::command]
fn set_compact_mode(window: Window, compact: bool) -> Result<(), String> {
    window
        .set_size(window_logical_size(compact))
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn settle_edge_dock(window: Window) -> Result<EdgeDockState, String> {
    if let Some(edge) = edge_from_cursor_or_window(&window, None)? {
        return set_edge_dock(&window, edge);
    }

    Ok(EdgeDockState {
        docked: false,
        edge: None,
    })
}

#[tauri::command]
fn finish_edge_drag(
    window: Window,
    runtime: State<'_, EdgeRuntimeState>,
) -> Result<EdgeDockState, String> {
    finish_edge_drag_state(&window, &runtime)
}

fn finish_edge_drag_state(
    window: &Window,
    runtime: &EdgeRuntimeState,
) -> Result<EdgeDockState, String> {
    let (current_edge, drag_compact) = edge_runtime_snapshot(runtime);
    if let Ok(mut state) = runtime.0.lock() {
        state.drag_active = false;
    }

    let next_edge = edge_from_cursor_or_window(window, Some(runtime))?
        .or_else(|| edge_from_cursor_or_window(window, None).ok().flatten());

    if let Some(edge) = next_edge {
        let state = set_edge_dock(window, edge)?;
        set_current_edge(runtime, Some(edge.to_string()));
        return Ok(state);
    }

    if let Some(edge) = current_edge {
        expand_from_drag_position(window, &edge, drag_compact, runtime)?;
        set_current_edge(runtime, None);
    }

    Ok(EdgeDockState {
        docked: false,
        edge: None,
    })
}

fn auto_settle_edge_dock(window: &Window, runtime: &EdgeRuntimeState) {
    if is_drag_active(runtime) {
        return;
    }

    let (current_edge, drag_compact) = edge_runtime_snapshot(runtime);
    let Ok(next_edge) = edge_from_cursor_or_window(window, Some(runtime)) else {
        return;
    };

    if current_edge.is_some() && next_edge.is_some() {
        return;
    }

    if let Some(edge) = next_edge {
        suppress_edge_settle_state(runtime);
        if let Ok(state) = set_edge_dock(window, edge) {
            set_current_edge(runtime, Some(edge.to_string()));
            let edge_arg = state.edge.as_deref().unwrap_or(edge);
            let _ = window.eval(&format!("window.applyEdgeDockFromHost?.({edge_arg:?})"));
        }
        return;
    }

    if let Some(edge) = current_edge {
        suppress_edge_settle_state(runtime);
        if expand_from_drag_position(window, &edge, drag_compact, runtime).is_ok() {
            set_current_edge(runtime, None);
            let _ = window.eval("window.applyEdgeUndockFromHost?.()");
        }
    }
}

fn schedule_edge_settle(app: AppHandle, label: String) {
    let runtime = app.state::<EdgeRuntimeState>();
    let generation = next_move_generation(&runtime);

    thread::spawn(move || {
        thread::sleep(Duration::from_millis(EDGE_MOVE_SETTLE_MS));

        let runtime = app.state::<EdgeRuntimeState>();
        let drag_active = is_drag_active(&runtime);
        if generation != current_move_generation(&runtime)
            || (!drag_active && is_edge_settle_suppressed(&runtime))
        {
            return;
        }

        if let Some(window) = app.get_window(&label) {
            if drag_active {
                if let Ok(state) = finish_edge_drag_state(&window, &runtime) {
                    if state.docked {
                        if let Some(edge) = state.edge {
                            let _ = window.eval(&format!("window.applyEdgeDockFromHost?.({edge:?})"));
                        }
                    } else {
                        let _ = window.eval("window.applyEdgeUndockFromHost?.()");
                    }
                }
            } else {
                auto_settle_edge_dock(&window, &runtime);
            }
        }
    });
}

#[tauri::command]
fn collapse_to_edge(
    window: Window,
    edge: String,
    runtime: State<'_, EdgeRuntimeState>,
) -> Result<EdgeDockState, String> {
    suppress_edge_settle(&runtime);
    let state = set_edge_dock(&window, &edge)?;
    set_current_edge(&runtime, Some(edge));
    Ok(state)
}

#[tauri::command]
fn expand_edge_dock(
    window: Window,
    edge: String,
    compact: bool,
    runtime: State<'_, EdgeRuntimeState>,
) -> Result<(), String> {
    suppress_edge_settle(&runtime);
    expand_from_edge(&window, &edge, compact)?;
    set_current_edge(&runtime, None);
    Ok(())
}

#[tauri::command]
fn set_data_source_cmd(app: AppHandle, source: String) -> Result<(), String> {
    match source.as_str() {
        "local" => {
            set_data_source(&app, DataSource::LocalLogs);
            Ok(())
        }
        "ccswitch" => {
            set_data_source(&app, DataSource::CcSwitch);
            Ok(())
        }
        _ => Err(format!("unknown source: {source}")),
    }
}

#[tauri::command]
fn refresh_cmd(app: AppHandle) {
    refresh_window(&app);
}

#[tauri::command]
fn close_menu_cmd(app: AppHandle) {
    if let Some(w) = app.get_window("context_menu") {
        let _ = w.hide();
        let _ = w.close();
    }
}

#[tauri::command]
fn menu_action(app: AppHandle, action: String) {
    match action.as_str() {
        "refresh" => refresh_window(&app),
        "source_local" => set_data_source(&app, DataSource::LocalLogs),
        "source_ccswitch" => set_data_source(&app, DataSource::CcSwitch),
        "quit" => app.exit(0),
        _ => {}
    }
    if let Some(w) = app.get_window("context_menu") {
        let _ = w.hide();
        let _ = w.close();
    }
}

#[tauri::command]
async fn show_context_menu(window: Window, x: f64, y: f64) {
    let app = window.app_handle();
    let source = app
        .state::<DataSourceState>()
        .0
        .lock()
        .map(|s| *s)
        .unwrap_or(DataSource::LocalLogs);
    let src_str = match source {
        DataSource::LocalLogs => "local",
        DataSource::CcSwitch => "ccswitch",
    };
    if let Some(old) = app.get_window("context_menu") {
        let _ = old.eval(&format!("window.showAt({x},{y},\"{src_str}\")"));
        return;
    }
    let init = format!("window.__INIT_SOURCE__=\"{src_str}\";");
    let _ = tauri::WindowBuilder::new(
        &window,
        "context_menu",
        tauri::WindowUrl::App("menu.html".into()),
    )
    .title("")
    .inner_size(200.0, 140.0)
    .position(x, y)
    .decorations(false)
    .transparent(true)
    .always_on_top(true)
    .resizable(false)
    .skip_taskbar(true)
    .focused(true)
    .initialization_script(&init)
    .build();
}

fn main() {
    let show = CustomMenuItem::new("show".to_string(), "Show / Hide");
    let refresh = CustomMenuItem::new("refresh".to_string(), "Refresh");
    let source_local = CustomMenuItem::new("source_local".to_string(), "[x] Local Codex/Claude");
    let source_ccswitch = CustomMenuItem::new("source_ccswitch".to_string(), "[ ] cc-switch");
    let quit = CustomMenuItem::new("quit".to_string(), "Quit");
    let tray = SystemTray::new().with_menu(
        SystemTrayMenu::new()
            .add_item(show)
            .add_item(refresh)
            .add_item(source_local)
            .add_item(source_ccswitch)
            .add_item(quit),
    );

    tauri::Builder::default()
        .manage(PinState(AtomicBool::new(true)))
        .manage(DataSourceState(Mutex::new(DataSource::LocalLogs)))
        .manage(LocalUsageCacheState(Mutex::new(LocalUsageCache::default())))
        .manage(EdgeRuntimeState(Mutex::new(EdgeRuntime {
            suppress_until: None,
            move_generation: 0,
            drag_cursor_offset_x: None,
            drag_cursor_offset_y: None,
            last_cursor_x: None,
            drag_compact: false,
            current_edge: None,
            drag_active: false,
        })))
        .system_tray(tray)
        .invoke_handler(tauri::generate_handler![
            get_stats,
            get_models,
            get_data_source,
            set_data_source_from_window,
            quit_app,
            show_native_context_menu,
            begin_edge_drag,
            drag_move_window,
            finish_edge_drag,
            undock_and_start_drag,
            hide_window,
            start_dragging,
            set_compact_mode,
            settle_edge_dock,
            collapse_to_edge,
            expand_edge_dock,
            toggle_top,
            show_context_menu,
            set_data_source_cmd,
            refresh_cmd,
            close_menu_cmd,
            menu_action
        ])
        .setup(|app| {
            let window = app
                .get_window("main")
                .ok_or_else(|| "missing main window".to_string())?;
            if let Some(state) = read_window_state(&app.handle()) {
                let _ = window.set_position(PhysicalPosition::new(state.x, state.y));
            }
            update_source_menu(&app.handle(), DataSource::LocalLogs);
            let _ = app.tray_handle().set_tooltip("Token Pet");
            window.show()?;
            Ok(())
        })
        .on_window_event(|event| {
            if let WindowEvent::Moved(position) = event.event() {
                let app_handle = event.window().app_handle();
                save_window_state(&app_handle, *position);
                let runtime = app_handle.state::<EdgeRuntimeState>();
                keep_window_in_monitor(event.window(), &runtime);
                if is_drag_active(&runtime) || !is_edge_settle_suppressed(&runtime) {
                    schedule_edge_settle(app_handle, event.window().label().to_string());
                }
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
                    refresh_window(app);
                }
                "source_local" => set_data_source(app, DataSource::LocalLogs),
                "source_ccswitch" => set_data_source(app, DataSource::CcSwitch),
                "quit" => app.exit(0),
                _ => {}
            },
            _ => {}
        })
        .run(tauri::generate_context!())
        .expect("error while running Token Pet");
}
