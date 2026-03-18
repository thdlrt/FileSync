use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RuleKind {
    File,
    Folder,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ConflictPolicy {
    OverwriteWithBackup,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DeletePolicy {
    NoDelete,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SyncTrigger {
    Manual,
    Watch,
    Poll,
    Startup,
    Cleanup,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RuleHealth {
    Healthy,
    MissingSource,
    InvalidSourceType,
    InvalidTargetPath,
    OverlappingDirectories,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RuleLastResult {
    pub success: bool,
    pub message: String,
    pub trigger: Option<SyncTrigger>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub copied_count: usize,
    pub updated_count: usize,
    pub skipped_count: usize,
    pub backup_count: usize,
    pub deleted_count: usize,
    pub error_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncRule {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub kind: RuleKind,
    pub source_path: String,
    pub target_path: String,
    pub auto_sync: bool,
    pub watch_enabled: bool,
    pub poll_fallback_enabled: bool,
    pub poll_interval_sec: u64,
    pub conflict_policy: ConflictPolicy,
    pub delete_policy: DeletePolicy,
    pub include_globs: Vec<String>,
    pub exclude_globs: Vec<String>,
    pub last_sync_at: Option<String>,
    #[serde(default)]
    pub last_result: RuleLastResult,
    #[serde(default = "default_health")]
    pub health: RuleHealth,
}

fn default_health() -> RuleHealth {
    RuleHealth::Healthy
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleDraft {
    pub name: String,
    pub enabled: bool,
    pub kind: RuleKind,
    pub source_path: String,
    pub target_path: String,
    pub auto_sync: bool,
    pub watch_enabled: bool,
    pub poll_fallback_enabled: bool,
    pub poll_interval_sec: u64,
    pub include_globs: Vec<String>,
    pub exclude_globs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    pub launch_on_startup: bool,
    pub start_minimized_to_tray: bool,
    pub close_to_tray: bool,
    pub theme: String,
    pub show_notifications: bool,
    pub default_poll_interval_sec: u64,
    pub backup_retention_days: u64,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            launch_on_startup: false,
            start_minimized_to_tray: false,
            close_to_tray: true,
            theme: "light".to_string(),
            show_notifications: true,
            default_poll_interval_sec: 300,
            backup_retention_days: 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncHistoryItem {
    pub rule_id: String,
    pub rule_name: String,
    pub started_at: String,
    pub finished_at: String,
    pub trigger: SyncTrigger,
    pub copied_count: usize,
    pub updated_count: usize,
    pub skipped_count: usize,
    pub backup_count: usize,
    pub deleted_count: usize,
    pub error_count: usize,
    pub success: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DashboardSummary {
    pub total_rules: usize,
    pub enabled_rules: usize,
    pub invalid_rules: usize,
    pub last_sync_at: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AppStateSnapshot {
    pub summary: DashboardSummary,
    pub rules: Vec<SyncRule>,
    pub settings: AppSettings,
    pub history: Vec<SyncHistoryItem>,
    pub automatic_sync_paused: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CleanupCandidateKind {
    File,
    Folder,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupCandidate {
    pub path: String,
    pub relative_path: String,
    pub kind: CleanupCandidateKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CleanupPreview {
    pub rule_id: String,
    pub rule_name: String,
    pub candidates: Vec<CleanupCandidate>,
    pub file_count: usize,
    pub folder_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppLogEntry {
    pub timestamp: String,
    pub level: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PersistedStore {
    pub rules: Vec<SyncRule>,
    pub settings: AppSettings,
    pub history: Vec<SyncHistoryItem>,
}

impl Default for SyncRule {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            enabled: true,
            kind: RuleKind::File,
            source_path: String::new(),
            target_path: String::new(),
            auto_sync: true,
            watch_enabled: true,
            poll_fallback_enabled: true,
            poll_interval_sec: 300,
            conflict_policy: ConflictPolicy::OverwriteWithBackup,
            delete_policy: DeletePolicy::NoDelete,
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
            last_sync_at: None,
            last_result: RuleLastResult::default(),
            health: RuleHealth::Healthy,
        }
    }
}
