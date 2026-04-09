use crate::models::{
    AppLogEntry, AppSettings, AppStateSnapshot, CleanupPreview, DashboardSummary, PersistedStore,
    RuleDraft, RuleHealth, RuleKind, RuleLastResult, RuleSyncState, SyncHistoryItem, SyncRule,
    SyncTrigger,
};
use crate::sync_engine;
use chrono::Utc;
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Manager, Wry};
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use uuid::Uuid;

#[derive(Clone)]
pub struct SharedState {
    inner: Arc<InnerState>,
}

struct InnerState {
    app: AppHandle<Wry>,
    store: Mutex<PersistedStore>,
    watchers: Mutex<HashMap<String, RuleWatcher>>,
    watcher_failures: Mutex<HashMap<String, String>>,
    runtime: Mutex<HashMap<String, RuleRuntime>>,
    automatic_sync_paused: AtomicBool,
}

struct RuleWatcher {
    _watcher: RecommendedWatcher,
}

#[derive(Default)]
struct RuleRuntime {
    in_progress: bool,
    rerun_requested: bool,
    queued_trigger: Option<SyncTrigger>,
    debounce_generation: u64,
    last_poll_at: Option<Instant>,
}

struct StoreLoadOutcome {
    store: PersistedStore,
    repaired: bool,
    warnings: Vec<String>,
}

impl SharedState {
    pub fn new(app: AppHandle<Wry>) -> Result<Self, String> {
        let store = load_store(&app)?;
        Ok(Self {
            inner: Arc::new(InnerState {
                app,
                store: Mutex::new(store),
                watchers: Mutex::new(HashMap::new()),
                watcher_failures: Mutex::new(HashMap::new()),
                runtime: Mutex::new(HashMap::new()),
                automatic_sync_paused: AtomicBool::new(false),
            }),
        })
    }

    pub async fn initialize(&self) -> Result<(), String> {
        self.recompute_health_and_save().await?;
        self.refresh_watchers().await?;
        self.start_poll_loop();
        self.write_log("info", "FileSync Notes 已启动。");
        Ok(())
    }

    pub async fn snapshot(&self) -> Result<AppStateSnapshot, String> {
        self.recompute_health_and_save().await?;
        let store = self.inner.store.lock().await;
        let mut rules = store.rules.clone();
        let history = store.history.clone();
        let settings = store.settings.clone();
        drop(store);

        let watcher_failures = self.inner.watcher_failures.lock().await.clone();
        apply_watcher_failures_to_rules(&mut rules, &watcher_failures);

        let automatic_sync_paused = self.inner.automatic_sync_paused.load(Ordering::Relaxed);

        let summary = DashboardSummary {
            total_rules: rules.len(),
            enabled_rules: rules.iter().filter(|rule| rule.enabled).count(),
            invalid_rules: rules
                .iter()
                .filter(|rule| rule.health != RuleHealth::Healthy)
                .count(),
            last_sync_at: rules
                .iter()
                .filter_map(|rule| rule.last_sync_at.clone())
                .max(),
            last_error: rules
                .iter()
                .filter(|rule| !rule.last_result.success && !rule.last_result.message.is_empty())
                .map(|rule| format!("{}：{}", rule.name, rule.last_result.message))
                .next(),
        };

        Ok(AppStateSnapshot {
            summary,
            rules,
            settings,
            history,
            automatic_sync_paused,
        })
    }

    pub async fn list_rules(&self) -> Result<Vec<SyncRule>, String> {
        Ok(self.snapshot().await?.rules)
    }

    pub async fn get_settings(&self) -> Result<AppSettings, String> {
        Ok(self.snapshot().await?.settings)
    }

    pub async fn get_logs(&self) -> Result<Vec<AppLogEntry>, String> {
        read_logs(&self.inner.app)
    }

    pub async fn clear_history(&self) -> Result<AppStateSnapshot, String> {
        {
            let mut store = self.inner.store.lock().await;
            store.history.clear();
            save_store(&self.inner.app, &store)?;
        }
        self.write_log("info", "已清空同步历史。");
        self.snapshot().await
    }

    pub async fn clear_logs(&self) -> Result<Vec<AppLogEntry>, String> {
        clear_logs(&self.inner.app)?;
        self.write_log("info", "已清空应用日志。");
        read_logs(&self.inner.app)
    }

    pub fn get_log_path(&self) -> Result<String, String> {
        Ok(log_file_path(&self.inner.app)?
            .to_string_lossy()
            .to_string())
    }

    pub async fn create_rule(&self, draft: RuleDraft) -> Result<AppStateSnapshot, String> {
        let settings = { self.inner.store.lock().await.settings.clone() };
        let rule = sync_engine::build_rule(None, draft, &settings)?;

        {
            let mut store = self.inner.store.lock().await;
            store.rules.push(rule);
            save_store(&self.inner.app, &store)?;
        }
        self.write_log("info", "已创建新的同步规则。");

        self.recompute_health_and_save().await?;
        self.refresh_watchers().await?;
        self.snapshot().await
    }

    pub async fn update_rule(
        &self,
        rule_id: String,
        draft: RuleDraft,
    ) -> Result<AppStateSnapshot, String> {
        let settings = { self.inner.store.lock().await.settings.clone() };
        let updated_rule = sync_engine::build_rule(Some(rule_id.clone()), draft, &settings)?;

        {
            let mut store = self.inner.store.lock().await;
            let existing = store
                .rules
                .iter_mut()
                .find(|rule| rule.id == rule_id)
                .ok_or_else(|| "未找到要更新的规则。".to_string())?;
            let last_sync_at = existing.last_sync_at.clone();
            let last_result = existing.last_result.clone();
            *existing = updated_rule;
            existing.last_sync_at = last_sync_at;
            existing.last_result = last_result;
            save_store(&self.inner.app, &store)?;
        }
        self.write_log("info", format!("已更新规则：{}", rule_id));

        self.recompute_health_and_save().await?;
        self.refresh_watchers().await?;
        self.snapshot().await
    }

    pub async fn delete_rule(&self, rule_id: String) -> Result<AppStateSnapshot, String> {
        {
            let mut store = self.inner.store.lock().await;
            store.rules.retain(|rule| rule.id != rule_id);
            save_store(&self.inner.app, &store)?;
        }
        self.write_log("info", "已删除一条同步规则。");

        self.inner.watchers.lock().await.remove(&rule_id);
        self.inner.runtime.lock().await.remove(&rule_id);
        self.snapshot().await
    }

    pub async fn toggle_rule_enabled(
        &self,
        rule_id: String,
        enabled: bool,
    ) -> Result<AppStateSnapshot, String> {
        {
            let mut store = self.inner.store.lock().await;
            let rule = store
                .rules
                .iter_mut()
                .find(|rule| rule.id == rule_id)
                .ok_or_else(|| "未找到要切换的规则。".to_string())?;
            rule.enabled = enabled;
            save_store(&self.inner.app, &store)?;
        }
        self.write_log(
            "info",
            if enabled {
                format!("已启用规则：{}", rule_id)
            } else {
                format!("已停用规则：{}", rule_id)
            },
        );

        self.recompute_health_and_save().await?;
        self.refresh_watchers().await?;
        self.snapshot().await
    }

    pub async fn save_settings(&self, settings: AppSettings) -> Result<AppStateSnapshot, String> {
        {
            let mut store = self.inner.store.lock().await;
            store.settings = settings;
            save_store(&self.inner.app, &store)?;
        }
        self.write_log("info", "应用设置已保存。");
        self.snapshot().await
    }

    pub async fn run_rule_sync_command(&self, rule_id: String) -> Result<AppStateSnapshot, String> {
        self.run_rule_sync_now(&rule_id, SyncTrigger::Manual)
            .await?;
        self.snapshot().await
    }

    pub async fn run_all_sync_command(&self) -> Result<AppStateSnapshot, String> {
        let rule_ids = {
            self.inner
                .store
                .lock()
                .await
                .rules
                .iter()
                .filter(|rule| rule.enabled)
                .map(|rule| rule.id.clone())
                .collect::<Vec<_>>()
        };

        for rule_id in rule_ids {
            let _ = self.run_rule_sync_now(&rule_id, SyncTrigger::Manual).await;
        }

        self.snapshot().await
    }

    pub async fn preview_cleanup(&self, rule_id: String) -> Result<CleanupPreview, String> {
        let rule = self.get_rule(&rule_id).await?;
        sync_engine::preview_rule_cleanup(&rule)
    }

    pub async fn execute_cleanup(&self, rule_id: String) -> Result<AppStateSnapshot, String> {
        let rule = self.get_rule(&rule_id).await?;
        let outcome = sync_engine::execute_rule_cleanup(&rule)?;
        self.apply_run_outcome(&rule_id, outcome).await?;
        self.snapshot().await
    }

    pub async fn set_auto_sync_paused(&self, paused: bool) -> Result<AppStateSnapshot, String> {
        self.inner
            .automatic_sync_paused
            .store(paused, Ordering::Relaxed);
        self.write_log(
            "info",
            if paused {
                "自动同步已暂停。"
            } else {
                "自动同步已恢复。"
            },
        );
        self.snapshot().await
    }

    fn write_log(&self, level: &str, message: impl Into<String>) {
        let _ = append_log(&self.inner.app, level, &message.into());
    }

    pub fn schedule_watch_sync(&self, rule_id: String) {
        let state = self.clone();
        tauri::async_runtime::spawn(async move {
            let generation = {
                let mut runtime = state.inner.runtime.lock().await;
                let entry = runtime.entry(rule_id.clone()).or_default();
                entry.debounce_generation += 1;
                entry.debounce_generation
            };

            tokio::time::sleep(Duration::from_millis(1500)).await;

            let should_run = {
                let runtime = state.inner.runtime.lock().await;
                runtime
                    .get(&rule_id)
                    .map(|entry| entry.debounce_generation == generation)
                    .unwrap_or(false)
            };

            if should_run && !state.inner.automatic_sync_paused.load(Ordering::Relaxed) {
                let _ = state
                    .enqueue_background_sync(rule_id, SyncTrigger::Watch)
                    .await;
            }
        });
    }

    async fn enqueue_background_sync(
        &self,
        rule_id: String,
        trigger: SyncTrigger,
    ) -> Result<(), String> {
        let should_spawn = {
            let mut runtime = self.inner.runtime.lock().await;
            let entry = runtime.entry(rule_id.clone()).or_default();
            if entry.in_progress {
                entry.rerun_requested = true;
                entry.queued_trigger = Some(trigger.clone());
                false
            } else {
                entry.in_progress = true;
                true
            }
        };

        if !should_spawn {
            return Ok(());
        }

        let state = self.clone();
        tauri::async_runtime::spawn(async move {
            let mut current_trigger = trigger;
            loop {
                let _ = state
                    .run_rule_sync_no_queue(&rule_id, current_trigger.clone())
                    .await;

                let next_trigger = {
                    let mut runtime = state.inner.runtime.lock().await;
                    let entry = runtime.entry(rule_id.clone()).or_default();
                    if entry.rerun_requested {
                        entry.rerun_requested = false;
                        entry.queued_trigger.take().or(Some(SyncTrigger::Watch))
                    } else {
                        entry.in_progress = false;
                        None
                    }
                };

                match next_trigger {
                    Some(trigger) => current_trigger = trigger,
                    None => break,
                }
            }
        });

        Ok(())
    }

    async fn run_rule_sync_now(&self, rule_id: &str, trigger: SyncTrigger) -> Result<(), String> {
        let already_running = {
            let mut runtime = self.inner.runtime.lock().await;
            let entry = runtime.entry(rule_id.to_string()).or_default();
            if entry.in_progress {
                true
            } else {
                entry.in_progress = true;
                false
            }
        };

        if already_running {
            return Err("该规则正在同步中，请稍候再试。".to_string());
        }

        let result = self.run_rule_sync_no_queue(rule_id, trigger).await;

        self.inner
            .runtime
            .lock()
            .await
            .entry(rule_id.to_string())
            .or_default()
            .in_progress = false;

        result
    }

    async fn run_rule_sync_no_queue(
        &self,
        rule_id: &str,
        trigger: SyncTrigger,
    ) -> Result<(), String> {
        if self.inner.automatic_sync_paused.load(Ordering::Relaxed)
            && matches!(trigger, SyncTrigger::Watch | SyncTrigger::Poll)
        {
            return Ok(());
        }

        let rule = self.get_rule(rule_id).await?;
        if !rule.enabled {
            return Ok(());
        }

        self.write_log(
            "info",
            format!(
                "开始执行规则“{}”，触发方式：{:?}，源：{}，目标：{}",
                rule.name, trigger, rule.source_path, rule.target_path
            ),
        );
        let settings = { self.inner.store.lock().await.settings.clone() };
        let outcome = sync_engine::run_rule_sync(&rule, &settings, trigger);
        self.apply_run_outcome(rule_id, outcome).await
    }

    async fn apply_run_outcome(
        &self,
        rule_id: &str,
        outcome: sync_engine::RuleRunOutcome,
    ) -> Result<(), String> {
        let mut store = self.inner.store.lock().await;
        if let Some(rule) = store.rules.iter_mut().find(|rule| rule.id == rule_id) {
            rule.last_sync_at = outcome.last_result.finished_at.clone();
            rule.last_result = outcome.last_result;
            if rule.last_result.success {
                if let Ok(sync_state) = sync_engine::capture_rule_sync_state(rule) {
                    rule.sync_state = sync_state;
                }
            }
            rule.health = sync_engine::evaluate_rule_health(rule);
        }

        let log_message = if outcome.history_item.success {
            format!(
                "规则“{}”执行成功，触发方式：{:?}，消息：{}",
                outcome.history_item.rule_name,
                outcome.history_item.trigger,
                outcome.history_item.message
            )
        } else {
            format!(
                "规则“{}”执行失败，触发方式：{:?}，消息：{}",
                outcome.history_item.rule_name,
                outcome.history_item.trigger,
                outcome.history_item.message
            )
        };

        store.history.insert(0, outcome.history_item);
        store.history.truncate(200);
        save_store(&self.inner.app, &store)?;
        drop(store);
        self.write_log(
            if log_message.contains("失败") {
                "error"
            } else {
                "info"
            },
            log_message,
        );
        Ok(())
    }

    async fn get_rule(&self, rule_id: &str) -> Result<SyncRule, String> {
        let store = self.inner.store.lock().await;
        store
            .rules
            .iter()
            .find(|rule| rule.id == rule_id)
            .cloned()
            .ok_or_else(|| "未找到对应的同步规则。".to_string())
    }

    async fn recompute_health_and_save(&self) -> Result<(), String> {
        let mut store = self.inner.store.lock().await;
        for rule in &mut store.rules {
            rule.health = sync_engine::evaluate_rule_health(rule);
        }
        save_store(&self.inner.app, &store)
    }

    async fn refresh_watchers(&self) -> Result<(), String> {
        let rules = { self.inner.store.lock().await.rules.clone() };
        let mut next_watchers = HashMap::new();
        let mut next_failures = HashMap::new();
        let mut warnings = Vec::new();

        'rules: for rule in rules {
            if !rule.enabled
                || !rule.auto_sync
                || !rule.watch_enabled
                || rule.health != RuleHealth::Healthy
            {
                continue;
            }

            let watch_targets = match collect_watch_targets(&rule) {
                Ok(targets) => targets,
                Err(error) => {
                    let message = format!("监听不可用：{error}");
                    next_failures.insert(rule.id.clone(), message.clone());
                    warnings.push(format!(
                        "规则“{}”的自动监听已降级，不会阻止应用启动。{}",
                        rule.name, message
                    ));
                    continue;
                }
            };
            let source_path = PathBuf::from(&rule.source_path);
            let target_path = PathBuf::from(&rule.target_path);
            let state = self.clone();
            let rule_id = rule.id.clone();
            let rule_kind = rule.kind.clone();
            let bidirectional = rule.bidirectional;

            let mut watcher = match RecommendedWatcher::new(
                move |event: Result<Event, notify::Error>| {
                    if let Ok(event) = event {
                        let relevant = event.paths.iter().any(|path| match rule_kind {
                            crate::models::RuleKind::File => {
                                path == &source_path || (bidirectional && path == &target_path)
                            }
                            crate::models::RuleKind::Folder => {
                                path.starts_with(&source_path)
                                    || (bidirectional && path.starts_with(&target_path))
                            }
                        });

                        if relevant {
                            state.schedule_watch_sync(rule_id.clone());
                        }
                    }
                },
                Config::default(),
            ) {
                Ok(watcher) => watcher,
                Err(error) => {
                    let message = format!("监听器创建失败：{error}");
                    next_failures.insert(rule.id.clone(), message.clone());
                    warnings.push(format!(
                        "规则“{}”的自动监听已降级，不会阻止应用启动。{}",
                        rule.name, message
                    ));
                    continue;
                }
            };

            for (watch_path, recursive_mode) in watch_targets {
                if let Err(error) = watcher.watch(&watch_path, recursive_mode) {
                    let message = format!("无法监听路径 {}：{}", watch_path.display(), error);
                    next_failures.insert(rule.id.clone(), message.clone());
                    warnings.push(format!(
                        "规则“{}”的自动监听已降级，不会阻止应用启动。{}",
                        rule.name, message
                    ));
                    continue 'rules;
                }
            }

            next_watchers.insert(rule.id.clone(), RuleWatcher { _watcher: watcher });
        }

        {
            let mut watchers = self.inner.watchers.lock().await;
            *watchers = next_watchers;
        }

        {
            let mut watcher_failures = self.inner.watcher_failures.lock().await;
            *watcher_failures = next_failures;
        }

        for warning in warnings {
            self.write_log("warn", warning);
        }

        Ok(())
    }

    fn start_poll_loop(&self) {
        let state = self.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                if state.inner.automatic_sync_paused.load(Ordering::Relaxed) {
                    continue;
                }

                let rules = {
                    state
                        .inner
                        .store
                        .lock()
                        .await
                        .rules
                        .iter()
                        .filter(|rule| {
                            rule.enabled
                                && rule.auto_sync
                                && rule.poll_fallback_enabled
                                && rule.health == RuleHealth::Healthy
                        })
                        .cloned()
                        .collect::<Vec<_>>()
                };

                for rule in rules {
                    let should_run = {
                        let mut runtime = state.inner.runtime.lock().await;
                        let entry = runtime.entry(rule.id.clone()).or_default();
                        let now = Instant::now();
                        match entry.last_poll_at {
                            Some(last)
                                if now.duration_since(last).as_secs() < rule.poll_interval_sec =>
                            {
                                false
                            }
                            _ => {
                                entry.last_poll_at = Some(now);
                                true
                            }
                        }
                    };

                    if should_run {
                        let _ = state
                            .enqueue_background_sync(rule.id.clone(), SyncTrigger::Poll)
                            .await;
                    }
                }
            }
        });
    }
}

fn collect_watch_targets(rule: &SyncRule) -> Result<Vec<(PathBuf, RecursiveMode)>, String> {
    let mut targets = Vec::new();
    let mut seen = BTreeSet::new();

    append_watch_target(&mut targets, &mut seen, Path::new(&rule.source_path), &rule.kind)?;
    if rule.bidirectional {
        append_watch_target(&mut targets, &mut seen, Path::new(&rule.target_path), &rule.kind)?;
    }

    if targets.is_empty() {
        return Err("当前没有可监听的已存在路径，请先创建任一侧目录或开启轮询兜底。".to_string());
    }

    Ok(targets)
}

fn append_watch_target(
    targets: &mut Vec<(PathBuf, RecursiveMode)>,
    seen: &mut BTreeSet<String>,
    path: &Path,
    kind: &RuleKind,
) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }

    let (watch_path, recursive_mode) = match kind {
        RuleKind::File => (
            path.parent()
                .map(Path::to_path_buf)
                .ok_or_else(|| "文件路径缺少父目录，无法监听。".to_string())?,
            RecursiveMode::NonRecursive,
        ),
        RuleKind::Folder => (path.to_path_buf(), RecursiveMode::Recursive),
    };

    let recursive = matches!(recursive_mode, RecursiveMode::Recursive);
    let key = format!("{}::{recursive}", watch_path.display());
    if seen.insert(key) {
        targets.push((watch_path, recursive_mode));
    }

    Ok(())
}

fn store_file_path(app: &AppHandle<Wry>) -> Result<PathBuf, String> {
    let app_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    fs::create_dir_all(&app_dir).map_err(|error| error.to_string())?;
    Ok(app_dir.join("store.json"))
}

fn apply_watcher_failures_to_rules(
    rules: &mut [SyncRule],
    watcher_failures: &HashMap<String, String>,
) {
    for rule in rules {
        if let Some(message) = watcher_failures.get(&rule.id) {
            if rule.health == RuleHealth::Healthy {
                rule.health = RuleHealth::WatchUnavailable;
            }

            if rule.config_error.is_none() {
                rule.config_error = Some(message.clone());
            }
        }
    }
}

fn load_store(app: &AppHandle<Wry>) -> Result<PersistedStore, String> {
    let store_path = store_file_path(app)?;
    if !store_path.exists() {
        let store = PersistedStore::default();
        save_store(app, &store)?;
        return Ok(store);
    }

    let content = fs::read_to_string(&store_path).map_err(|error| error.to_string())?;
    let outcome = deserialize_store_lossy(&content);

    if outcome.repaired {
        let backup_note = match backup_store_file(&store_path) {
            Ok(path) => format!("原配置已备份到 {}", path.display()),
            Err(error) => format!("原配置备份失败：{error}"),
        };

        for warning in &outcome.warnings {
            let _ = append_log(
                app,
                "warn",
                &format!("检测到配置异常，已自动修复。{backup_note}。{warning}"),
            );
        }

        save_store(app, &outcome.store)?;
    }

    Ok(outcome.store)
}

fn deserialize_store_lossy(content: &str) -> StoreLoadOutcome {
    let parsed = match serde_json::from_str::<Value>(content) {
        Ok(value) => value,
        Err(error) => {
            let settings = AppSettings::default();
            let message = format!("配置文件无法解析：{error}");
            return StoreLoadOutcome {
                store: PersistedStore {
                    rules: vec![build_invalid_rule(
                        None,
                        None,
                        RuleKind::File,
                        None,
                        None,
                        true,
                        false,
                        false,
                        false,
                        settings.default_poll_interval_sec,
                        Vec::new(),
                        Vec::new(),
                        None,
                        RuleLastResult::default(),
                        vec![message.clone()],
                    )],
                    settings,
                    history: Vec::new(),
                },
                repaired: true,
                warnings: vec![message],
            };
        }
    };

    let object = match parsed.as_object() {
        Some(object) => object,
        None => {
            let settings = AppSettings::default();
            let message = "配置文件根节点不是对象，已重建默认配置。".to_string();
            return StoreLoadOutcome {
                store: PersistedStore {
                    rules: vec![build_invalid_rule(
                        None,
                        Some("已损坏的规则配置".to_string()),
                        RuleKind::File,
                        None,
                        None,
                        true,
                        false,
                        false,
                        false,
                        settings.default_poll_interval_sec,
                        Vec::new(),
                        Vec::new(),
                        None,
                        RuleLastResult::default(),
                        vec![message.clone()],
                    )],
                    settings,
                    history: Vec::new(),
                },
                repaired: true,
                warnings: vec![message],
            };
        }
    };

    let mut warnings = Vec::new();
    let settings = parse_settings(object.get("settings"), &mut warnings);
    let rules = parse_rules(object.get("rules"), &settings, &mut warnings);
    let history = parse_history(object.get("history"), &mut warnings);

    StoreLoadOutcome {
        store: PersistedStore {
            rules,
            settings,
            history,
        },
        repaired: !warnings.is_empty(),
        warnings,
    }
}

fn parse_settings(value: Option<&Value>, warnings: &mut Vec<String>) -> AppSettings {
    match value {
        Some(raw) => serde_json::from_value::<AppSettings>(raw.clone()).unwrap_or_else(|error| {
            warnings.push(format!("设置项格式异常，已恢复默认值：{error}"));
            AppSettings::default()
        }),
        None => AppSettings::default(),
    }
}

fn parse_rules(
    value: Option<&Value>,
    settings: &AppSettings,
    warnings: &mut Vec<String>,
) -> Vec<SyncRule> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(index, item)| parse_rule(item, index, settings, warnings))
            .collect(),
        Some(_) => {
            let message = "规则列表格式异常，已标记为损坏规则。".to_string();
            warnings.push(message.clone());
            vec![build_invalid_rule(
                None,
                Some("已损坏的规则配置".to_string()),
                RuleKind::File,
                None,
                None,
                true,
                false,
                false,
                false,
                settings.default_poll_interval_sec,
                Vec::new(),
                Vec::new(),
                None,
                RuleLastResult::default(),
                vec![message],
            )]
        }
        None => Vec::new(),
    }
}

fn parse_rule(
    value: &Value,
    index: usize,
    settings: &AppSettings,
    warnings: &mut Vec<String>,
) -> SyncRule {
    let Some(object) = value.as_object() else {
        let message = format!("第 {} 条规则不是有效对象，已标记为异常。", index + 1);
        warnings.push(message.clone());
        return build_invalid_rule(
            None,
            Some(format!("异常规则 {}", index + 1)),
            RuleKind::File,
            None,
            None,
            true,
            false,
            false,
            false,
            settings.default_poll_interval_sec,
            Vec::new(),
            Vec::new(),
            None,
            RuleLastResult::default(),
            vec![message],
        );
    };

    let mut issues = Vec::new();
    let id = read_string_field(object, "id", &mut issues)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            issues.push("缺少规则 ID，已重新生成。".to_string());
            Uuid::new_v4().to_string()
        });
    let name = read_string_field(object, "name", &mut issues)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            issues.push("缺少规则名称，已使用默认名称。".to_string());
            format!("异常规则 {}", index + 1)
        });
    let kind = parse_rule_kind(read_string_field(object, "kind", &mut issues), &mut issues);
    let source_path = read_string_field(object, "sourcePath", &mut issues).unwrap_or_default();
    if source_path.trim().is_empty() {
        issues.push("源路径缺失。".to_string());
    }
    let target_path = read_string_field(object, "targetPath", &mut issues).unwrap_or_default();
    if target_path.trim().is_empty() {
        issues.push("目标路径缺失。".to_string());
    }
    let enabled = read_bool_field(object, "enabled", true, &mut issues);
    let bidirectional = read_bool_field(object, "bidirectional", false, &mut issues);
    let auto_sync = read_bool_field(object, "autoSync", true, &mut issues);
    let watch_enabled = read_bool_field(object, "watchEnabled", true, &mut issues);
    let poll_fallback_enabled = read_bool_field(object, "pollFallbackEnabled", true, &mut issues);
    let poll_interval_sec = read_u64_field(
        object,
        "pollIntervalSec",
        settings.default_poll_interval_sec.max(5),
        &mut issues,
    )
    .max(5);
    let include_globs = read_string_array_field(object, "includeGlobs", &mut issues);
    let exclude_globs = read_string_array_field(object, "excludeGlobs", &mut issues);
    let sync_state = parse_sync_state(object.get("syncState"), &mut issues);
    let last_sync_at = read_optional_string_field(object, "lastSyncAt", &mut issues);
    let mut last_result = parse_last_result(object.get("lastResult"), &mut issues);
    let delete_policy = sync_engine::default_delete_policy(kind.clone(), bidirectional);

    if issues.is_empty() {
        return SyncRule {
            id,
            name,
            enabled,
            kind,
            source_path,
            target_path,
            bidirectional,
            auto_sync,
            watch_enabled,
            poll_fallback_enabled,
            poll_interval_sec,
            conflict_policy: crate::models::ConflictPolicy::OverwriteWithBackup,
            delete_policy,
            include_globs,
            exclude_globs,
            sync_state,
            last_sync_at,
            last_result,
            health: RuleHealth::Healthy,
            config_error: None,
        };
    }

    let issue_text = issues.join(" ");
    warnings.push(format!(
        "规则“{}”存在配置异常，已按异常规则加载。{}",
        name, issue_text
    ));
    if last_result.message.trim().is_empty() {
        last_result.message = format!("规则配置异常：{issue_text}");
    } else {
        last_result.message = format!("{} | 配置异常：{issue_text}", last_result.message);
    }
    last_result.success = false;
    last_result.error_count = last_result.error_count.max(1);

    build_invalid_rule(
        Some(id),
        Some(name),
        kind,
        if source_path.trim().is_empty() {
            None
        } else {
            Some(source_path)
        },
        if target_path.trim().is_empty() {
            None
        } else {
            Some(target_path)
        },
        enabled,
        bidirectional,
        auto_sync,
        poll_fallback_enabled,
        poll_interval_sec,
        include_globs,
        exclude_globs,
        last_sync_at,
        last_result,
        issues,
    )
}

fn parse_history(value: Option<&Value>, warnings: &mut Vec<String>) -> Vec<SyncHistoryItem> {
    match value {
        Some(Value::Array(items)) => {
            let mut history = Vec::new();
            for (index, item) in items.iter().enumerate() {
                match serde_json::from_value::<SyncHistoryItem>(item.clone()) {
                    Ok(entry) => history.push(entry),
                    Err(error) => warnings.push(format!(
                        "第 {} 条历史记录格式异常，已跳过：{}",
                        index + 1,
                        error
                    )),
                }
            }
            history
        }
        Some(_) => {
            warnings.push("同步历史格式异常，已重置为空。".to_string());
            Vec::new()
        }
        None => Vec::new(),
    }
}

fn read_string_field(
    object: &Map<String, Value>,
    key: &str,
    issues: &mut Vec<String>,
) -> Option<String> {
    match object.get(key) {
        Some(Value::String(value)) => Some(value.clone()),
        Some(Value::Null) => None,
        Some(_) => {
            issues.push(format!("字段“{}”不是字符串。", key));
            None
        }
        None => None,
    }
}

fn read_optional_string_field(
    object: &Map<String, Value>,
    key: &str,
    issues: &mut Vec<String>,
) -> Option<String> {
    read_string_field(object, key, issues).filter(|value| !value.trim().is_empty())
}

fn read_bool_field(
    object: &Map<String, Value>,
    key: &str,
    default: bool,
    issues: &mut Vec<String>,
) -> bool {
    match object.get(key) {
        Some(Value::Bool(value)) => *value,
        Some(Value::Null) => default,
        Some(_) => {
            issues.push(format!("字段“{}”不是布尔值。", key));
            default
        }
        None => default,
    }
}

fn read_u64_field(
    object: &Map<String, Value>,
    key: &str,
    default: u64,
    issues: &mut Vec<String>,
) -> u64 {
    match object.get(key) {
        Some(Value::Number(value)) => value.as_u64().unwrap_or_else(|| {
            issues.push(format!("字段“{}”不是正整数。", key));
            default
        }),
        Some(Value::Null) => default,
        Some(_) => {
            issues.push(format!("字段“{}”不是数字。", key));
            default
        }
        None => default,
    }
}

fn read_string_array_field(
    object: &Map<String, Value>,
    key: &str,
    issues: &mut Vec<String>,
) -> Vec<String> {
    match object.get(key) {
        Some(Value::Array(items)) => {
            let mut result = Vec::new();
            let mut invalid_count = 0usize;
            for item in items {
                match item {
                    Value::String(value) => {
                        let trimmed = value.trim();
                        if !trimmed.is_empty() {
                            result.push(trimmed.to_string());
                        }
                    }
                    Value::Null => {}
                    _ => invalid_count += 1,
                }
            }
            if invalid_count > 0 {
                issues.push(format!(
                    "字段“{}”中有 {} 项不是字符串，已忽略。",
                    key, invalid_count
                ));
            }
            result
        }
        Some(Value::Null) => Vec::new(),
        Some(_) => {
            issues.push(format!("字段“{}”不是字符串数组。", key));
            Vec::new()
        }
        None => Vec::new(),
    }
}

fn parse_rule_kind(raw_kind: Option<String>, issues: &mut Vec<String>) -> RuleKind {
    match raw_kind.as_deref() {
        Some("file") => RuleKind::File,
        Some("folder") => RuleKind::Folder,
        Some(other) => {
            issues.push(format!("规则类型“{}”无效，已按文件规则载入。", other));
            RuleKind::File
        }
        None => {
            issues.push("缺少规则类型，已按文件规则载入。".to_string());
            RuleKind::File
        }
    }
}

fn parse_last_result(value: Option<&Value>, issues: &mut Vec<String>) -> RuleLastResult {
    match value {
        Some(raw) => {
            serde_json::from_value::<RuleLastResult>(raw.clone()).unwrap_or_else(|error| {
                issues.push(format!("最近一次同步结果格式异常，已重置：{error}"));
                RuleLastResult::default()
            })
        }
        None => RuleLastResult::default(),
    }
}

fn parse_sync_state(value: Option<&Value>, issues: &mut Vec<String>) -> RuleSyncState {
    match value {
        Some(raw) => serde_json::from_value::<RuleSyncState>(raw.clone()).unwrap_or_else(|error| {
            issues.push(format!("同步清单格式异常，已重置：{error}"));
            RuleSyncState::default()
        }),
        None => RuleSyncState::default(),
    }
}

fn build_invalid_rule(
    id: Option<String>,
    name: Option<String>,
    kind: RuleKind,
    source_path: Option<String>,
    target_path: Option<String>,
    enabled: bool,
    bidirectional: bool,
    auto_sync: bool,
    poll_fallback_enabled: bool,
    poll_interval_sec: u64,
    include_globs: Vec<String>,
    exclude_globs: Vec<String>,
    last_sync_at: Option<String>,
    mut last_result: RuleLastResult,
    issues: Vec<String>,
) -> SyncRule {
    let issue_text = issues.join(" ");
    if last_result.message.trim().is_empty() {
        last_result.message = format!("规则配置异常：{issue_text}");
    }
    last_result.success = false;
    last_result.error_count = last_result.error_count.max(1);
    let delete_policy = sync_engine::default_delete_policy(kind.clone(), bidirectional);

    SyncRule {
        id: id.unwrap_or_else(|| Uuid::new_v4().to_string()),
        name: name.unwrap_or_else(|| "已损坏的规则配置".to_string()),
        enabled,
        kind,
        source_path: source_path.unwrap_or_default(),
        target_path: target_path.unwrap_or_default(),
        bidirectional,
        auto_sync,
        watch_enabled: false,
        poll_fallback_enabled,
        poll_interval_sec: poll_interval_sec.max(5),
        conflict_policy: crate::models::ConflictPolicy::OverwriteWithBackup,
        delete_policy,
        include_globs,
        exclude_globs,
        sync_state: RuleSyncState::default(),
        last_sync_at,
        last_result,
        health: RuleHealth::InvalidConfiguration,
        config_error: Some(issue_text),
    }
}

fn backup_store_file(store_path: &Path) -> Result<PathBuf, String> {
    let backup_path = store_path.with_file_name(format!(
        "store.corrupt.{}.json",
        Utc::now().timestamp_millis()
    ));
    fs::copy(store_path, &backup_path).map_err(|error| error.to_string())?;
    Ok(backup_path)
}

fn save_store(app: &AppHandle<Wry>, store: &PersistedStore) -> Result<(), String> {
    let store_path = store_file_path(app)?;
    let temp_path = store_path.with_extension(format!("{}.tmp", Utc::now().timestamp_millis()));
    let json = serde_json::to_string_pretty(store).map_err(|error| error.to_string())?;
    fs::write(&temp_path, json).map_err(|error| error.to_string())?;
    fs::rename(&temp_path, &store_path)
        .or_else(|_| {
            if store_path.exists() {
                fs::remove_file(&store_path)?;
            }
            fs::rename(&temp_path, &store_path)
        })
        .map_err(|error| error.to_string())
}

fn log_file_path(app: &AppHandle<Wry>) -> Result<PathBuf, String> {
    let app_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    fs::create_dir_all(&app_dir).map_err(|error| error.to_string())?;
    Ok(app_dir.join("app.log.jsonl"))
}

fn append_log(app: &AppHandle<Wry>, level: &str, message: &str) -> Result<(), String> {
    let path = log_file_path(app)?;
    let entry = AppLogEntry {
        timestamp: Utc::now().to_rfc3339(),
        level: level.to_string(),
        message: message.to_string(),
    };
    let serialized = serde_json::to_string(&entry).map_err(|error| error.to_string())?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| error.to_string())?;
    writeln!(file, "{serialized}").map_err(|error| error.to_string())
}

fn read_logs(app: &AppHandle<Wry>) -> Result<Vec<AppLogEntry>, String> {
    let path = log_file_path(app)?;
    if !path.exists() {
        return Ok(Vec::new());
    }

    let content = fs::read_to_string(&path).map_err(|error| error.to_string())?;
    let mut entries = content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<AppLogEntry>(line).ok())
        .collect::<Vec<_>>();
    entries.reverse();
    Ok(entries)
}

fn clear_logs(app: &AppHandle<Wry>) -> Result<(), String> {
    let path = log_file_path(app)?;
    if path.exists() {
        fs::remove_file(path).map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{apply_watcher_failures_to_rules, deserialize_store_lossy};

    #[test]
    fn deserialize_store_marks_invalid_rules_without_failing() {
        let content = r#"
        {
          "settings": {
            "launchOnStartup": false,
            "startMinimizedToTray": false,
            "closeToTray": true,
            "theme": "light",
            "showNotifications": true,
            "defaultPollIntervalSec": 300,
            "backupRetentionDays": 30
          },
          "rules": [
            {
              "id": "broken-rule",
              "name": "损坏规则",
              "enabled": true,
              "kind": "mystery",
              "sourcePath": "C:\\\\source.txt",
              "targetPath": "C:\\\\target.txt",
              "autoSync": true,
              "watchEnabled": true,
              "pollFallbackEnabled": true,
              "pollIntervalSec": 60,
              "includeGlobs": [],
              "excludeGlobs": []
            }
          ],
          "history": []
        }
        "#;

        let outcome = deserialize_store_lossy(content);

        assert!(outcome.repaired);
        assert_eq!(outcome.store.rules.len(), 1);
        assert_eq!(
            outcome.store.rules[0].health,
            crate::models::RuleHealth::InvalidConfiguration
        );
        assert!(outcome.store.rules[0].config_error.is_some());
    }

    #[test]
    fn deserialize_store_recovers_from_invalid_json() {
        let outcome = deserialize_store_lossy("{ not-json");

        assert!(outcome.repaired);
        assert_eq!(outcome.store.rules.len(), 1);
        assert_eq!(
            outcome.store.rules[0].health,
            crate::models::RuleHealth::InvalidConfiguration
        );
        assert!(
            outcome.store.rules[0]
                .last_result
                .message
                .contains("无法解析")
        );
    }

    #[test]
    fn watcher_failures_only_mark_rule_unavailable() {
        let mut rule = crate::models::SyncRule::default();
        rule.id = "rule-1".to_string();
        rule.name = "示例规则".to_string();

        let mut rules = vec![rule];
        let failures = std::collections::HashMap::from([(
            "rule-1".to_string(),
            "监听不可用：Access is denied.".to_string(),
        )]);

        apply_watcher_failures_to_rules(&mut rules, &failures);

        assert_eq!(rules[0].health, crate::models::RuleHealth::WatchUnavailable);
        assert_eq!(
            rules[0].config_error.as_deref(),
            Some("监听不可用：Access is denied.")
        );
    }
}
