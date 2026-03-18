use crate::models::{
    AppLogEntry, AppSettings, AppStateSnapshot, CleanupPreview, DashboardSummary, PersistedStore,
    RuleDraft, RuleHealth, SyncRule, SyncTrigger,
};
use crate::sync_engine;
use chrono::Utc;
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Manager, Wry};
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};

#[derive(Clone)]
pub struct SharedState {
    inner: Arc<InnerState>,
}

struct InnerState {
    app: AppHandle<Wry>,
    store: Mutex<PersistedStore>,
    watchers: Mutex<HashMap<String, RuleWatcher>>,
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

impl SharedState {
    pub fn new(app: AppHandle<Wry>) -> Result<Self, String> {
        let store = load_store(&app)?;
        Ok(Self {
            inner: Arc::new(InnerState {
                app,
                store: Mutex::new(store),
                watchers: Mutex::new(HashMap::new()),
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
        let rules = store.rules.clone();
        let history = store.history.clone();
        let settings = store.settings.clone();
        let automatic_sync_paused = self.inner.automatic_sync_paused.load(Ordering::Relaxed);
        drop(store);

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
        Ok(log_file_path(&self.inner.app)?.to_string_lossy().to_string())
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
            rule.health = sync_engine::evaluate_rule_health(rule);
        }

        let log_message = if outcome.history_item.success {
            format!(
                "规则“{}”执行成功，触发方式：{:?}，消息：{}",
                outcome.history_item.rule_name, outcome.history_item.trigger, outcome.history_item.message
            )
        } else {
            format!(
                "规则“{}”执行失败，触发方式：{:?}，消息：{}",
                outcome.history_item.rule_name, outcome.history_item.trigger, outcome.history_item.message
            )
        };

        store.history.insert(0, outcome.history_item);
        store.history.truncate(200);
        save_store(&self.inner.app, &store)?;
        drop(store);
        self.write_log(
            if log_message.contains("失败") { "error" } else { "info" },
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
        let mut watchers = self.inner.watchers.lock().await;
        watchers.clear();

        for rule in rules {
            if !rule.enabled
                || !rule.auto_sync
                || !rule.watch_enabled
                || rule.health != RuleHealth::Healthy
            {
                continue;
            }

            let watch_path = match rule.kind {
                crate::models::RuleKind::File => PathBuf::from(&rule.source_path)
                    .parent()
                    .map(Path::to_path_buf)
                    .ok_or_else(|| "源文件缺少父目录，无法监听。".to_string())?,
                crate::models::RuleKind::Folder => PathBuf::from(&rule.source_path),
            };
            let source_path = PathBuf::from(&rule.source_path);
            let state = self.clone();
            let rule_id = rule.id.clone();
            let rule_kind = rule.kind.clone();

            let mut watcher = RecommendedWatcher::new(
                move |event: Result<Event, notify::Error>| {
                    if let Ok(event) = event {
                        let relevant = event.paths.iter().any(|path| match rule_kind {
                            crate::models::RuleKind::File => path == &source_path,
                            crate::models::RuleKind::Folder => path.starts_with(&source_path),
                        });

                        if relevant {
                            state.schedule_watch_sync(rule_id.clone());
                        }
                    }
                },
                Config::default(),
            )
            .map_err(|error| error.to_string())?;

            watcher
                .watch(
                    &watch_path,
                    if matches!(rule.kind, crate::models::RuleKind::Folder) {
                        RecursiveMode::Recursive
                    } else {
                        RecursiveMode::NonRecursive
                    },
                )
                .map_err(|error| error.to_string())?;

            watchers.insert(rule.id.clone(), RuleWatcher { _watcher: watcher });
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

fn store_file_path(app: &AppHandle<Wry>) -> Result<PathBuf, String> {
    let app_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    fs::create_dir_all(&app_dir).map_err(|error| error.to_string())?;
    Ok(app_dir.join("store.json"))
}

fn load_store(app: &AppHandle<Wry>) -> Result<PersistedStore, String> {
    let store_path = store_file_path(app)?;
    if !store_path.exists() {
        let store = PersistedStore::default();
        save_store(app, &store)?;
        return Ok(store);
    }

    let content = fs::read_to_string(store_path).map_err(|error| error.to_string())?;
    serde_json::from_str(&content).map_err(|error| error.to_string())
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
