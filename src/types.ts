export type RuleKind = "file" | "folder";
export type RuleHealth =
  | "healthy"
  | "invalidConfiguration"
  | "missingSource"
  | "invalidSourceType"
  | "invalidTargetPath"
  | "overlappingDirectories"
  | "watchUnavailable";
export type SyncTrigger = "manual" | "watch" | "poll" | "startup" | "cleanup";
export type CleanupCandidateKind = "file" | "folder";

export interface RuleLastResult {
  success: boolean;
  message: string;
  trigger: SyncTrigger | null;
  startedAt: string | null;
  finishedAt: string | null;
  copiedCount: number;
  updatedCount: number;
  skippedCount: number;
  backupCount: number;
  deletedCount: number;
  errorCount: number;
}

export interface SyncRule {
  id: string;
  name: string;
  enabled: boolean;
  kind: RuleKind;
  sourcePath: string;
  targetPath: string;
  bidirectional: boolean;
  autoSync: boolean;
  watchEnabled: boolean;
  pollFallbackEnabled: boolean;
  pollIntervalSec: number;
  includeGlobs: string[];
  excludeGlobs: string[];
  lastSyncAt: string | null;
  lastResult: RuleLastResult;
  health: RuleHealth;
  configError: string | null;
}

export interface RuleDraft {
  name: string;
  enabled: boolean;
  kind: RuleKind;
  sourcePath: string;
  targetPath: string;
  bidirectional: boolean;
  autoSync: boolean;
  watchEnabled: boolean;
  pollFallbackEnabled: boolean;
  pollIntervalSec: number;
  includeGlobs: string[];
  excludeGlobs: string[];
}

export interface AppSettings {
  launchOnStartup: boolean;
  startMinimizedToTray: boolean;
  closeToTray: boolean;
  theme: string;
  showNotifications: boolean;
  defaultPollIntervalSec: number;
  backupRetentionDays: number;
}

export interface SyncHistoryItem {
  ruleId: string;
  ruleName: string;
  startedAt: string;
  finishedAt: string;
  trigger: SyncTrigger;
  copiedCount: number;
  updatedCount: number;
  skippedCount: number;
  backupCount: number;
  deletedCount: number;
  errorCount: number;
  success: boolean;
  message: string;
}

export interface DashboardSummary {
  totalRules: number;
  enabledRules: number;
  invalidRules: number;
  lastSyncAt: string | null;
  lastError: string | null;
}

export interface AppStateSnapshot {
  summary: DashboardSummary;
  rules: SyncRule[];
  settings: AppSettings;
  history: SyncHistoryItem[];
  automaticSyncPaused: boolean;
}

export interface CleanupCandidate {
  path: string;
  relativePath: string;
  kind: CleanupCandidateKind;
}

export interface CleanupPreview {
  ruleId: string;
  ruleName: string;
  candidates: CleanupCandidate[];
  fileCount: number;
  folderCount: number;
}

export interface AppLogEntry {
  timestamp: string;
  level: string;
  message: string;
}
