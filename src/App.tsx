import { FormEvent, startTransition, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { confirm, message, open, save } from "@tauri-apps/plugin-dialog";
import { enable, disable, isEnabled as isAutostartEnabled } from "@tauri-apps/plugin-autostart";
import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
} from "@tauri-apps/plugin-notification";
import "./App.css";
import type {
  AppLogEntry,
  AppSettings,
  AppStateSnapshot,
  CleanupPreview,
  RuleDraft,
  RuleKind,
  SyncRule,
} from "./types";

type Page = "dashboard" | "history" | "settings";
type SettingsTab = "general" | "logs";

const emptySettings: AppSettings = {
  launchOnStartup: false,
  startMinimizedToTray: false,
  closeToTray: true,
  theme: "light",
  showNotifications: true,
  defaultPollIntervalSec: 300,
  backupRetentionDays: 30,
};

const createEmptyDraft = (settings: AppSettings): RuleDraft => ({
  name: "",
  enabled: true,
  kind: "file",
  sourcePath: "",
  targetPath: "",
  autoSync: true,
  watchEnabled: true,
  pollFallbackEnabled: true,
  pollIntervalSec: settings.defaultPollIntervalSec || 300,
  includeGlobs: [],
  excludeGlobs: [],
});

function App() {
  const [page, setPage] = useState<Page>("dashboard");
  const [snapshot, setSnapshot] = useState<AppStateSnapshot | null>(null);
  const [settingsDraft, setSettingsDraft] = useState<AppSettings>(emptySettings);
  const [ruleDraft, setRuleDraft] = useState<RuleDraft>(createEmptyDraft(emptySettings));
  const [logs, setLogs] = useState<AppLogEntry[]>([]);
  const [logPath, setLogPath] = useState("");
  const [settingsTab, setSettingsTab] = useState<SettingsTab>("general");
  const [settingsDirty, setSettingsDirty] = useState(false);
  const [editingRuleId, setEditingRuleId] = useState<string | null>(null);
  const [isRuleModalOpen, setIsRuleModalOpen] = useState(false);
  const [cleanupPreview, setCleanupPreview] = useState<CleanupPreview | null>(null);
  const [busyAction, setBusyAction] = useState<string | null>(null);
  const [statusMessage, setStatusMessage] = useState<string | null>(null);
  const [statusTone, setStatusTone] = useState<"neutral" | "success" | "error">("neutral");
  const statusTimerRef = useRef<number | null>(null);

  const rules = snapshot?.rules ?? [];
  const history = snapshot?.history ?? [];
  const summary = snapshot?.summary;
  const automaticSyncPaused = snapshot?.automaticSyncPaused ?? false;
  const draftTargetDetails = useMemo(() => {
    if (ruleDraft.kind !== "file") {
      return null;
    }
    return describeFileTargetPath(ruleDraft.targetPath);
  }, [ruleDraft.kind, ruleDraft.targetPath]);

  const sortedRules = useMemo(
    () =>
      [...rules].sort((left, right) => {
        if (left.enabled !== right.enabled) {
          return left.enabled ? -1 : 1;
        }
        return left.name.localeCompare(right.name, "zh-CN");
      }),
    [rules]
  );

  useEffect(() => {
    void loadSnapshot();
  }, []);

  useEffect(() => {
    if (page === "settings" && settingsTab === "logs") {
      void loadLogs();
    }
  }, [page, settingsTab]);

  useEffect(() => {
    if (isRuleModalOpen || cleanupPreview) {
      return;
    }
    const timer = window.setInterval(() => {
      void refreshSnapshotSilently();
    }, 3000);
    return () => window.clearInterval(timer);
  }, [cleanupPreview, isRuleModalOpen]);

  async function invokeSnapshot<T>(command: string, payload?: Record<string, unknown>) {
    return invoke<T>(command, payload);
  }

  async function loadSnapshot() {
    try {
      const latest = await invokeSnapshot<AppStateSnapshot>("get_app_state");
      const autostartEnabled = await safeGetAutostartEnabled();
      latest.settings.launchOnStartup = autostartEnabled;
      applySnapshot(latest);
    } catch (error) {
      flashStatus(formatError(error), "error");
    }
  }

  async function loadLogs() {
    try {
      const [entries, currentLogPath] = await Promise.all([
        invokeSnapshot<AppLogEntry[]>("get_logs"),
        invokeSnapshot<string>("get_log_path"),
      ]);
      setLogs(entries);
      setLogPath(currentLogPath);
    } catch (error) {
      flashStatus(formatError(error), "error");
    }
  }

  async function refreshSnapshotSilently() {
    try {
      const latest = await invokeSnapshot<AppStateSnapshot>("get_app_state");
      const autostartEnabled = await safeGetAutostartEnabled();
      latest.settings.launchOnStartup = autostartEnabled;
      applySnapshot(latest);
    } catch {
      // Ignore background refresh failures.
    }
  }

  function applySnapshot(nextSnapshot: AppStateSnapshot) {
    startTransition(() => {
      setSnapshot(nextSnapshot);
      if (!settingsDirty) {
        setSettingsDraft(nextSnapshot.settings);
      }
      if (!isRuleModalOpen && !editingRuleId) {
        setRuleDraft(createEmptyDraft(nextSnapshot.settings));
      }
    });
  }

  function flashStatus(messageText: string, tone: "neutral" | "success" | "error") {
    if (statusTimerRef.current) {
      window.clearTimeout(statusTimerRef.current);
      statusTimerRef.current = null;
    }
    setStatusMessage(messageText);
    setStatusTone(tone);
    if (tone !== "error") {
      statusTimerRef.current = window.setTimeout(() => {
        setStatusMessage(null);
        statusTimerRef.current = null;
      }, 4500);
    }
  }

  function dismissStatus() {
    if (statusTimerRef.current) {
      window.clearTimeout(statusTimerRef.current);
      statusTimerRef.current = null;
    }
    setStatusMessage(null);
  }

  function updateSettings<K extends keyof AppSettings>(key: K, value: AppSettings[K]) {
    setSettingsDirty(true);
    setSettingsDraft((current) => ({
      ...current,
      [key]: value,
    }));
  }

  function openCreateRuleModal() {
    setEditingRuleId(null);
    setRuleDraft(createEmptyDraft(snapshot?.settings ?? emptySettings));
    setIsRuleModalOpen(true);
  }

  function openEditRuleModal(rule: SyncRule) {
    setEditingRuleId(rule.id);
    setRuleDraft({
      name: rule.name,
      enabled: rule.enabled,
      kind: rule.kind,
      sourcePath: rule.sourcePath,
      targetPath: rule.targetPath,
      autoSync: rule.autoSync,
      watchEnabled: rule.watchEnabled,
      pollFallbackEnabled: rule.pollFallbackEnabled,
      pollIntervalSec: rule.pollIntervalSec,
      includeGlobs: rule.includeGlobs,
      excludeGlobs: rule.excludeGlobs,
    });
    setIsRuleModalOpen(true);
  }

  async function pickSourcePath(kind: RuleKind) {
    const selected = await open({
      multiple: false,
      directory: kind === "folder",
    });
    if (typeof selected === "string") {
      setRuleDraft((current) => ({ ...current, sourcePath: selected }));
    }
  }

  async function pickTargetFilePath() {
    const selected = await save({
      defaultPath: ruleDraft.targetPath || undefined,
    });
    if (selected) {
      setRuleDraft((current) => ({ ...current, targetPath: selected }));
    }
  }

  async function pickTargetDirectoryPath() {
    const selected = await open({
      multiple: false,
      directory: true,
    });
    if (typeof selected !== "string") {
      return;
    }

    const nextTargetPath =
      ruleDraft.kind === "file"
        ? joinPath(selected, inferTargetFileName(ruleDraft))
        : selected;
    setRuleDraft((current) => ({ ...current, targetPath: nextTargetPath }));
  }

  async function handleSaveRule(event: FormEvent) {
    event.preventDefault();
    setBusyAction("save-rule");

    try {
      const payload = sanitizeDraft(ruleDraft);
      const nextSnapshot = editingRuleId
        ? await invokeSnapshot<AppStateSnapshot>("update_rule", {
            ruleId: editingRuleId,
            rule: payload,
          })
        : await invokeSnapshot<AppStateSnapshot>("create_rule", {
            rule: payload,
          });

      applySnapshot(nextSnapshot);
      setIsRuleModalOpen(false);
      setEditingRuleId(null);
      flashStatus(editingRuleId ? "规则已更新。" : "规则已创建。", "success");
    } catch (error) {
      flashStatus(formatError(error), "error");
      await message(formatError(error), { title: "保存失败", kind: "error" });
    } finally {
      setBusyAction(null);
    }
  }

  async function handleDeleteRule(rule: SyncRule) {
    const confirmed = await confirm(`确定删除规则“${rule.name}”吗？`, {
      title: "删除规则",
      kind: "warning",
    });
    if (!confirmed) {
      return;
    }

    setBusyAction(`delete-${rule.id}`);
    try {
      const nextSnapshot = await invokeSnapshot<AppStateSnapshot>("delete_rule", {
        ruleId: rule.id,
      });
      applySnapshot(nextSnapshot);
      flashStatus(`规则“${rule.name}”已删除。`, "success");
    } catch (error) {
      flashStatus(formatError(error), "error");
    } finally {
      setBusyAction(null);
    }
  }

  async function handleToggleRule(rule: SyncRule) {
    setBusyAction(`toggle-${rule.id}`);
    try {
      const nextSnapshot = await invokeSnapshot<AppStateSnapshot>("toggle_rule_enabled", {
        ruleId: rule.id,
        enabled: !rule.enabled,
      });
      applySnapshot(nextSnapshot);
      flashStatus(
        !rule.enabled ? `规则“${rule.name}”已启用。` : `规则“${rule.name}”已停用。`,
        "success"
      );
    } catch (error) {
      flashStatus(formatError(error), "error");
    } finally {
      setBusyAction(null);
    }
  }

  async function handleRunRule(rule: SyncRule) {
    setBusyAction(`run-${rule.id}`);
    try {
      const nextSnapshot = await invokeSnapshot<AppStateSnapshot>("run_rule_sync", {
        ruleId: rule.id,
      });
      applySnapshot(nextSnapshot);
      flashStatus(`规则“${rule.name}”同步完成。`, "success");
      await maybeNotify("FileSync Notes", `规则“${rule.name}”同步完成。`);
    } catch (error) {
      flashStatus(formatError(error), "error");
    } finally {
      setBusyAction(null);
    }
  }

  async function handleRunAll() {
    setBusyAction("run-all");
    try {
      const nextSnapshot = await invokeSnapshot<AppStateSnapshot>("run_all_sync");
      applySnapshot(nextSnapshot);
      flashStatus("全部规则同步完成。", "success");
      await maybeNotify("FileSync Notes", "全部规则同步完成。");
    } catch (error) {
      flashStatus(formatError(error), "error");
    } finally {
      setBusyAction(null);
    }
  }

  async function handlePreviewCleanup(rule: SyncRule) {
    setBusyAction(`preview-cleanup-${rule.id}`);
    try {
      const preview = await invokeSnapshot<CleanupPreview>("preview_rule_cleanup", {
        ruleId: rule.id,
      });
      if (preview.candidates.length === 0) {
        flashStatus(`规则“${rule.name}”当前没有可清理的备份。`, "neutral");
        return;
      }
      setCleanupPreview(preview);
      flashStatus(`已生成“${rule.name}”的备份清理预览。`, "success");
    } catch (error) {
      flashStatus(formatError(error), "error");
    } finally {
      setBusyAction(null);
    }
  }

  async function handleExecuteCleanup() {
    if (!cleanupPreview) {
      return;
    }

    const confirmed = await confirm(
      `确定删除 ${cleanupPreview.candidates.length} 个备份项目吗？此操作不会影响源文件和目标文件。`,
      {
        title: "确认清理备份",
        kind: "warning",
      }
    );
    if (!confirmed) {
      return;
    }

    setBusyAction(`cleanup-${cleanupPreview.ruleId}`);
    try {
      const nextSnapshot = await invokeSnapshot<AppStateSnapshot>("execute_rule_cleanup", {
        ruleId: cleanupPreview.ruleId,
      });
      applySnapshot(nextSnapshot);
      flashStatus(`规则“${cleanupPreview.ruleName}”的备份清理完成。`, "success");
      setCleanupPreview(null);
    } catch (error) {
      flashStatus(formatError(error), "error");
    } finally {
      setBusyAction(null);
    }
  }

  async function handleSaveSettings(event: FormEvent) {
    event.preventDefault();
    setBusyAction("save-settings");
    try {
      if (settingsDraft.launchOnStartup) {
        await enable();
      } else {
        await disable();
      }

      if (settingsDraft.showNotifications) {
        await ensureNotificationPermission();
      }

      const nextSnapshot = await invokeSnapshot<AppStateSnapshot>("save_settings", {
        settings: settingsDraft,
      });
      setSettingsDirty(false);
      applySnapshot(nextSnapshot);
      flashStatus("设置已保存。", "success");
    } catch (error) {
      flashStatus(formatError(error), "error");
    } finally {
      setBusyAction(null);
    }
  }

  async function handleTogglePause() {
    setBusyAction("toggle-pause");
    try {
      const nextSnapshot = await invokeSnapshot<AppStateSnapshot>("set_auto_sync_paused", {
        paused: !automaticSyncPaused,
      });
      applySnapshot(nextSnapshot);
      flashStatus(
        nextSnapshot.automaticSyncPaused ? "自动同步已暂停。" : "自动同步已恢复。",
        "success"
      );
    } catch (error) {
      flashStatus(formatError(error), "error");
    } finally {
      setBusyAction(null);
    }
  }

  async function handleClearLogs() {
    setBusyAction("clear-logs");
    try {
      const nextLogs = await invokeSnapshot<AppLogEntry[]>("clear_logs");
      setLogs(nextLogs);
      flashStatus("日志已清空。", "success");
    } catch (error) {
      flashStatus(formatError(error), "error");
    } finally {
      setBusyAction(null);
    }
  }

  async function handleClearHistory() {
    const confirmed = await confirm("确定清空全部同步历史吗？这不会影响规则和备份文件。", {
      title: "清空同步历史",
      kind: "warning",
    });
    if (!confirmed) {
      return;
    }

    setBusyAction("clear-history");
    try {
      const nextSnapshot = await invokeSnapshot<AppStateSnapshot>("clear_history");
      applySnapshot(nextSnapshot);
      flashStatus("同步历史已清空。", "success");
    } catch (error) {
      flashStatus(formatError(error), "error");
    } finally {
      setBusyAction(null);
    }
  }

  async function handleRevealPath(path: string) {
    try {
      await invokeSnapshot("reveal_path", { path });
    } catch (error) {
      flashStatus(formatError(error), "error");
    }
  }

  async function handleOpenLogFile() {
    if (!logPath) {
      return;
    }

    try {
      await invokeSnapshot("open_with_default_app", { path: logPath });
    } catch (error) {
      flashStatus(formatError(error), "error");
    }
  }

  return (
    <main className="app-shell">
      <aside className="sidebar">
        <div className="brand">
          <img className="brand-icon" src="/filesync-icon.png" alt="FileSync Notes icon" />
          <div>
            <h1>FileSync Notes</h1>
            <p>笔记到项目文档的轻量同步工具</p>
          </div>
        </div>

        <nav className="nav-stack">
          <button
            className={page === "dashboard" ? "nav-link active" : "nav-link"}
            onClick={() => setPage("dashboard")}
          >
            控制台
          </button>
          <button
            className={page === "history" ? "nav-link active" : "nav-link"}
            onClick={() => setPage("history")}
          >
            历史记录
          </button>
          <button
            className={page === "settings" ? "nav-link active" : "nav-link"}
            onClick={() => {
              setPage("settings");
              setSettingsTab("general");
            }}
          >
            设置
          </button>
        </nav>
      </aside>

      <section className="content-shell">
        <header className="topbar">
          <div>
            <h2>
              {page === "dashboard"
                ? "控制台"
                : page === "history"
                  ? "同步历史"
                  : settingsTab === "logs"
                    ? "设置 / 日志"
                    : "设置"}
            </h2>
            <p className="muted-text">
              {page === "dashboard"
                ? "查看同步状态、切换自动同步，并管理当前规则。"
                : page === "history"
                  ? "查看每次同步和清理动作的结果。"
                  : settingsTab === "logs"
                    ? "查看执行日志、失败原因和日志文件位置，方便排查问题。"
                    : "管理开机自启、托盘行为、通知和备份保留策略。"}
            </p>
          </div>

          {page === "dashboard" ? (
            <div className="action-row">
              <button
                className={automaticSyncPaused ? "flip-button paused" : "flip-button running"}
                onClick={handleTogglePause}
                disabled={busyAction === "toggle-pause"}
              >
                <span className="flip-indicator" />
                <span>
                  {busyAction === "toggle-pause"
                    ? "切换中..."
                    : automaticSyncPaused
                      ? "自动同步已暂停，点击恢复"
                      : "自动同步运行中，点击暂停"}
                </span>
              </button>
              <button className="secondary-button" onClick={handleRunAll} disabled={busyAction === "run-all"}>
                {busyAction === "run-all" ? "同步中..." : "立即全部同步"}
              </button>
              <button className="primary-button" onClick={openCreateRuleModal}>
                新建规则
              </button>
            </div>
          ) : page === "history" ? (
            <div className="action-row">
              <button
                className="danger-button"
                onClick={handleClearHistory}
                disabled={busyAction === "clear-history"}
              >
                {busyAction === "clear-history" ? "清空中..." : "清空历史"}
              </button>
            </div>
          ) : page === "settings" ? (
            <div className="settings-subnav">
              <button
                className={settingsTab === "general" ? "segment active" : "segment"}
                onClick={() => setSettingsTab("general")}
                type="button"
              >
                常规
              </button>
              <button
                className={settingsTab === "logs" ? "segment active" : "segment"}
                onClick={() => setSettingsTab("logs")}
                type="button"
              >
                日志
              </button>
            </div>
          ) : null}
        </header>

        {statusMessage ? (
          <div className={`status-banner ${statusTone}`}>
            <span>{statusMessage}</span>
            <button className="status-dismiss" type="button" onClick={dismissStatus}>
              关闭
            </button>
          </div>
        ) : null}

        {page === "dashboard" ? (
          <>
            <section className="console-status">
              <div className="console-status-copy">
                <span className="sidebar-label">后台状态</span>
                <strong>{automaticSyncPaused ? "自动同步已暂停" : "自动同步运行中"}</strong>
              </div>
              <div className="console-status-copy">
                <span className="sidebar-label">当前模式</span>
                <strong>{automaticSyncPaused ? "仅手动同步" : "监听 + 轮询"}</strong>
              </div>
            </section>

            <section className="summary-grid">
              <article className="summary-card">
                <span>规则总数</span>
                <strong>{summary?.totalRules ?? 0}</strong>
              </article>
              <article className="summary-card">
                <span>已启用规则</span>
                <strong>{summary?.enabledRules ?? 0}</strong>
              </article>
              <article className="summary-card">
                <span>异常规则</span>
                <strong>{summary?.invalidRules ?? 0}</strong>
              </article>
              <article className="summary-card wide">
                <span>最近一次同步</span>
                <strong>{formatDateTime(summary?.lastSyncAt)}</strong>
                <small>{summary?.lastError || "目前没有新的错误。"}</small>
              </article>
            </section>

            <section className="rules-section">
              {sortedRules.length === 0 ? (
                <div className="empty-state">
                  <h3>还没有同步规则</h3>
                  <p>先创建一条规则，把统一笔记目录映射到项目文档位置。</p>
                  <button className="primary-button" onClick={openCreateRuleModal}>
                    创建第一条规则
                  </button>
                </div>
              ) : (
                sortedRules.map((rule) => (
                  <article className="rule-card" key={rule.id}>
                    <div className="rule-card-top">
                      <div>
                        <div className="rule-title-row">
                          <h3>{rule.name}</h3>
                          <span className={`badge ${rule.enabled ? "success" : "muted"}`}>
                            {rule.enabled ? "已启用" : "已停用"}
                          </span>
                          <span className={`badge ${healthClassName(rule.health)}`}>
                            {healthLabel(rule.health)}
                          </span>
                          <span className="badge kind">{rule.kind === "file" ? "文件规则" : "文件夹规则"}</span>
                        </div>
                        <p className="path-row">
                          <span>源：</span>
                          {rule.sourcePath}
                        </p>
                        {rule.kind === "file" ? (
                          <div className="path-details">
                            <p className="path-row">
                              <span>目标文件：</span>
                              {getPathBaseName(rule.targetPath) || "未指定"}
                            </p>
                            <p className="path-row">
                              <span>目标目录：</span>
                              {getPathDirName(rule.targetPath) || "未指定"}
                            </p>
                            <p className="path-row compact">
                              <span>完整路径：</span>
                              {rule.targetPath}
                            </p>
                          </div>
                        ) : (
                          <p className="path-row">
                            <span>目标目录：</span>
                            {rule.targetPath}
                          </p>
                        )}
                      </div>

                      <div className="rule-actions">
                        <div className="rule-action-group">
                          <button className="secondary-button" onClick={() => handleToggleRule(rule)}>
                            {rule.enabled ? "停用" : "启用"}
                          </button>
                          <button className="secondary-button" onClick={() => handleRunRule(rule)}>
                            {busyAction === `run-${rule.id}` ? "同步中..." : "立即同步"}
                          </button>
                          <button className="secondary-button" onClick={() => openEditRuleModal(rule)}>
                            编辑
                          </button>
                          <button className="secondary-button" onClick={() => handlePreviewCleanup(rule)}>
                            清理备份
                          </button>
                        </div>
                        <div className="rule-action-group">
                          <button className="ghost-button" onClick={() => void handleRevealPath(rule.sourcePath)}>
                            打开源路径
                          </button>
                          <button className="ghost-button" onClick={() => void handleRevealPath(rule.targetPath)}>
                            打开目标路径
                          </button>
                          <button className="danger-button" onClick={() => handleDeleteRule(rule)}>
                            删除
                          </button>
                        </div>
                      </div>
                    </div>

                    <div className="rule-meta-grid">
                      <div>
                        <span>自动同步</span>
                        <strong>{rule.autoSync ? "开启" : "关闭"}</strong>
                      </div>
                      <div>
                        <span>监听变化</span>
                        <strong>{rule.watchEnabled ? "开启" : "关闭"}</strong>
                      </div>
                      <div>
                        <span>轮询兜底</span>
                        <strong>
                          {rule.pollFallbackEnabled ? `${rule.pollIntervalSec} 秒` : "关闭"}
                        </strong>
                      </div>
                      <div>
                        <span>上次同步</span>
                        <strong>{formatDateTime(rule.lastSyncAt)}</strong>
                      </div>
                    </div>

                    <div className="result-strip">
                      <strong>{rule.lastResult.message || "等待第一次同步"}</strong>
                      <small>
                        新增 {rule.lastResult.copiedCount} / 更新 {rule.lastResult.updatedCount} / 未变更{" "}
                        {rule.lastResult.skippedCount} / 备份 {rule.lastResult.backupCount}
                      </small>
                      <small>未变更表示目标内容已经是最新状态，这次无需覆盖。</small>
                    </div>
                  </article>
                ))
              )}
            </section>
          </>
        ) : null}

        {page === "history" ? (
          <section className="history-section">
            {history.length === 0 ? (
              <div className="empty-state compact">
                <h3>还没有同步历史</h3>
                <p>执行一次同步或清理后，这里会显示详细记录。</p>
              </div>
            ) : (
              <div className="history-table">
                {history.map((item, index) => (
                  <div className="history-row-compact" key={`${item.ruleId}-${item.startedAt}-${index}`}>
                    <span className={`badge ${item.success ? "success" : "error"}`}>
                      {item.success ? "成功" : "失败"}
                    </span>
                    <span className="history-name">{item.ruleName}</span>
                    <span className="history-time">{formatDateTime(item.finishedAt)}</span>
                    <span className="history-summary">
                      {triggerLabel(item.trigger)} / 新增 {item.copiedCount} / 更新 {item.updatedCount} /
                      未变更 {item.skippedCount} / 删除 {item.deletedCount}
                    </span>
                    <code className="history-inline">{item.message}</code>
                  </div>
                ))}
              </div>
            )}
          </section>
        ) : null}

        {page === "settings" ? (
          <section className="settings-section">
            {settingsTab === "general" ? (
              <form className="settings-form" onSubmit={handleSaveSettings}>
                <label className="toggle-row">
                  <div className="toggle-copy">
                    <strong>开机自启</strong>
                    <p>登录 Windows 后自动启动 FileSync Notes。</p>
                  </div>
                  <input
                    type="checkbox"
                    checked={settingsDraft.launchOnStartup}
                    onChange={(event) => updateSettings("launchOnStartup", event.currentTarget.checked)}
                  />
                </label>

                <label className="toggle-row">
                  <div className="toggle-copy">
                    <strong>启动后隐藏到托盘</strong>
                    <p>适合常驻后台运行。</p>
                  </div>
                  <input
                    type="checkbox"
                    checked={settingsDraft.startMinimizedToTray}
                    onChange={(event) => updateSettings("startMinimizedToTray", event.currentTarget.checked)}
                  />
                </label>

                <label className="toggle-row">
                  <div className="toggle-copy">
                    <strong>关闭主窗口时隐藏到托盘</strong>
                    <p>真正退出请使用托盘菜单或系统关闭。</p>
                  </div>
                  <input
                    type="checkbox"
                    checked={settingsDraft.closeToTray}
                    onChange={(event) => updateSettings("closeToTray", event.currentTarget.checked)}
                  />
                </label>

                <label className="toggle-row">
                  <div className="toggle-copy">
                    <strong>显示系统通知</strong>
                    <p>手动同步完成时发送 Windows 通知。</p>
                  </div>
                  <input
                    type="checkbox"
                    checked={settingsDraft.showNotifications}
                    onChange={(event) => updateSettings("showNotifications", event.currentTarget.checked)}
                  />
                </label>

                <div className="settings-grid">
                  <label className="field-group">
                    <span>默认轮询间隔（秒）</span>
                    <input
                      type="number"
                      min={5}
                      value={settingsDraft.defaultPollIntervalSec}
                      onChange={(event) =>
                        updateSettings("defaultPollIntervalSec", Number(event.currentTarget.value || 300))
                      }
                    />
                  </label>

                  <label className="field-group">
                    <span>备份保留时长（天）</span>
                    <input
                      type="number"
                      min={1}
                      value={settingsDraft.backupRetentionDays}
                      onChange={(event) =>
                        updateSettings("backupRetentionDays", Number(event.currentTarget.value || 30))
                      }
                    />
                  </label>
                </div>

                <button className="primary-button" type="submit">
                  {busyAction === "save-settings" ? "保存中..." : "保存设置"}
                </button>
              </form>
            ) : (
              <section className="logs-panel">
                <div className="log-toolbar">
                  <div className="log-toolbar-meta">
                    <strong>日志文件</strong>
                    <span>{logPath || "尚未生成日志文件"}</span>
                  </div>
                  <div className="action-row">
                    <button className="secondary-button" type="button" onClick={() => void loadLogs()}>
                      刷新
                    </button>
                    <button
                      className="ghost-button"
                      type="button"
                      onClick={() => void handleOpenLogFile()}
                      disabled={!logPath}
                    >
                      打开文件
                    </button>
                    <button
                      className="danger-button"
                      type="button"
                      onClick={handleClearLogs}
                      disabled={busyAction === "clear-logs"}
                    >
                      {busyAction === "clear-logs" ? "清空中..." : "清空日志"}
                    </button>
                  </div>
                </div>

                {logs.length === 0 ? (
                  <div className="empty-state compact">
                    <h3>还没有日志记录</h3>
                    <p>执行一次同步、保存设置或等待自动轮询后，这里会显示日志。</p>
                  </div>
                ) : (
                  <div className="log-table">
                    {logs.map((entry, index) => (
                      <div className="log-row" key={`${entry.timestamp}-${index}`}>
                        <span className={`badge ${entry.level === "error" ? "error" : "success"}`}>
                          {entry.level.toUpperCase()}
                        </span>
                        <span className="log-time">{formatDateTime(entry.timestamp)}</span>
                        <code className="log-inline">{entry.message}</code>
                      </div>
                    ))}
                  </div>
                )}
              </section>
            )}
          </section>
        ) : null}
      </section>

      {isRuleModalOpen ? (
        <div className="modal-backdrop" onClick={() => setIsRuleModalOpen(false)}>
          <div className="modal-panel" onClick={(event) => event.stopPropagation()}>
            <div className="modal-header">
              <div>
                <h3>{editingRuleId ? "编辑规则" : "新建规则"}</h3>
                <p>源路径必须存在，目标路径可以先不存在，首次同步会自动创建。</p>
              </div>
              <button className="ghost-button" onClick={() => setIsRuleModalOpen(false)}>
                关闭
              </button>
            </div>

            <form className="rule-form" onSubmit={handleSaveRule}>
              <label className="field-group">
                <span>规则名称</span>
                <input
                  value={ruleDraft.name}
                  onChange={(event) => {
                    const value = event.currentTarget.value;
                    setRuleDraft((current) => ({ ...current, name: value }));
                  }}
                  placeholder="例如：同步 README 到项目"
                  required
                />
              </label>

              <div className="segmented">
                <button
                  type="button"
                  className={ruleDraft.kind === "file" ? "segment active" : "segment"}
                  onClick={() =>
                    setRuleDraft((current) => ({
                      ...current,
                      kind: "file",
                      targetPath: current.kind === "folder" ? "" : current.targetPath,
                    }))
                  }
                >
                  文件规则
                </button>
                <button
                  type="button"
                  className={ruleDraft.kind === "folder" ? "segment active" : "segment"}
                  onClick={() =>
                    setRuleDraft((current) => ({
                      ...current,
                      kind: "folder",
                      targetPath: current.kind === "file" ? "" : current.targetPath,
                    }))
                  }
                >
                  文件夹规则
                </button>
              </div>

              <label className="field-group">
                <span>源路径</span>
                <div className="path-input-group">
                  <input
                    value={ruleDraft.sourcePath}
                    onChange={(event) => {
                      const value = event.currentTarget.value;
                      setRuleDraft((current) => ({
                        ...current,
                        sourcePath: value,
                      }));
                    }}
                    placeholder={ruleDraft.kind === "file" ? "选择现有源文件" : "选择现有源文件夹"}
                    required
                  />
                  <button
                    type="button"
                    className="secondary-button"
                    onClick={() => pickSourcePath(ruleDraft.kind)}
                  >
                    浏览
                  </button>
                </div>
              </label>

              <label className="field-group">
                <span>目标路径</span>
                <div className="path-input-group">
                  <input
                    value={ruleDraft.targetPath}
                    onChange={(event) => {
                      const value = event.currentTarget.value;
                      setRuleDraft((current) => ({
                        ...current,
                        targetPath: value,
                      }));
                    }}
                    placeholder={
                      ruleDraft.kind === "file"
                        ? "例如：E:\\project\\README.md"
                        : "例如：E:\\project\\docs"
                    }
                    required
                  />
                  <div className="path-button-group">
                    {ruleDraft.kind === "file" ? (
                      <>
                        <button
                          type="button"
                          className="secondary-button"
                          onClick={() => void pickTargetFilePath()}
                        >
                          选文件
                        </button>
                        <button
                          type="button"
                          className="ghost-button"
                          onClick={() => void pickTargetDirectoryPath()}
                        >
                          选文件夹
                        </button>
                      </>
                    ) : (
                      <button
                        type="button"
                        className="secondary-button"
                        onClick={() => void pickTargetDirectoryPath()}
                      >
                        选目录
                      </button>
                    )}
                  </div>
                </div>
                <small>
                  允许先不存在，首次同步会自动创建缺失目录和目标文件。
                  {ruleDraft.kind === "file"
                    ? " 也可以先选一个文件夹，程序会自动补上文件名。"
                    : ""}
                </small>
                {draftTargetDetails ? (
                  <div className="helper-note">
                    <strong>当前会写入的目标文件</strong>
                    <span>文件名：{draftTargetDetails.fileName || "未指定"}</span>
                    <span>目录：{draftTargetDetails.directory || "未指定"}</span>
                    {!draftTargetDetails.hasExtension ? (
                      <span>提示：当前文件名没有扩展名，首次同步会创建一个无扩展名文件。</span>
                    ) : null}
                  </div>
                ) : null}
              </label>

              <div className="inline-grid">
                <label className="toggle-card">
                  <span>启用规则</span>
                  <input
                    type="checkbox"
                    checked={ruleDraft.enabled}
                    onChange={(event) => {
                      const checked = event.currentTarget.checked;
                      setRuleDraft((current) => ({
                        ...current,
                        enabled: checked,
                      }));
                    }}
                  />
                </label>
                <label className="toggle-card">
                  <span>自动同步</span>
                  <input
                    type="checkbox"
                    checked={ruleDraft.autoSync}
                    onChange={(event) => {
                      const checked = event.currentTarget.checked;
                      setRuleDraft((current) => ({
                        ...current,
                        autoSync: checked,
                      }));
                    }}
                  />
                </label>
                <label className="toggle-card">
                  <span>监听变化</span>
                  <input
                    type="checkbox"
                    checked={ruleDraft.watchEnabled}
                    onChange={(event) => {
                      const checked = event.currentTarget.checked;
                      setRuleDraft((current) => ({
                        ...current,
                        watchEnabled: checked,
                      }));
                    }}
                  />
                </label>
                <label className="toggle-card">
                  <span>轮询兜底</span>
                  <input
                    type="checkbox"
                    checked={ruleDraft.pollFallbackEnabled}
                    onChange={(event) => {
                      const checked = event.currentTarget.checked;
                      setRuleDraft((current) => ({
                        ...current,
                        pollFallbackEnabled: checked,
                      }));
                    }}
                  />
                </label>
              </div>

              <label className="field-group">
                <span>轮询间隔（秒）</span>
                <input
                  type="number"
                  min={5}
                  value={ruleDraft.pollIntervalSec}
                  onChange={(event) => {
                    const value = Number(event.currentTarget.value || 300);
                    setRuleDraft((current) => ({
                      ...current,
                      pollIntervalSec: value,
                    }));
                  }}
                />
              </label>

              {ruleDraft.kind === "folder" ? (
                <>
                  <label className="field-group">
                    <span>包含规则（每行一个 glob，可留空）</span>
                    <textarea
                      rows={4}
                      value={ruleDraft.includeGlobs.join("\n")}
                      onChange={(event) => {
                        const value = splitLines(event.currentTarget.value);
                        setRuleDraft((current) => ({
                          ...current,
                          includeGlobs: value,
                        }));
                      }}
                      placeholder="例如：**/*.md"
                    />
                  </label>

                  <label className="field-group">
                    <span>排除规则（每行一个 glob，可留空）</span>
                    <textarea
                      rows={4}
                      value={ruleDraft.excludeGlobs.join("\n")}
                      onChange={(event) => {
                        const value = splitLines(event.currentTarget.value);
                        setRuleDraft((current) => ({
                          ...current,
                          excludeGlobs: value,
                        }));
                      }}
                      placeholder="例如：**/.back/**"
                    />
                  </label>
                </>
              ) : null}

              <div className="action-row end">
                <button type="button" className="ghost-button" onClick={() => setIsRuleModalOpen(false)}>
                  取消
                </button>
                <button className="primary-button" type="submit">
                  {busyAction === "save-rule" ? "保存中..." : editingRuleId ? "保存修改" : "创建规则"}
                </button>
              </div>
            </form>
          </div>
        </div>
      ) : null}

      {cleanupPreview ? (
        <div className="modal-backdrop" onClick={() => setCleanupPreview(null)}>
          <div className="modal-panel cleanup-panel" onClick={(event) => event.stopPropagation()}>
            <div className="modal-header">
              <div>
                <h3>备份清理预览：{cleanupPreview.ruleName}</h3>
                <p>
                  共发现 {cleanupPreview.fileCount} 个备份文件，{cleanupPreview.folderCount} 个备份目录。
                </p>
              </div>
            </div>

            <div className="cleanup-list">
              {cleanupPreview.candidates.length === 0 ? (
                <div className="empty-state compact">
                  <h3>当前没有可清理的备份</h3>
                  <p>备份目录已经是空的。</p>
                </div>
              ) : (
                cleanupPreview.candidates.map((candidate) => (
                  <div className="cleanup-row" key={`${candidate.kind}-${candidate.path}`}>
                    <span className={`badge ${candidate.kind === "folder" ? "kind" : "success"}`}>
                      {candidate.kind === "folder" ? "目录" : "文件"}
                    </span>
                    <div>
                      <strong>{candidate.relativePath}</strong>
                      <p>{candidate.path}</p>
                    </div>
                  </div>
                ))
              )}
            </div>

            <div className="action-row end">
              <button className="ghost-button" onClick={() => setCleanupPreview(null)}>
                取消
              </button>
              <button
                className="danger-button"
                onClick={handleExecuteCleanup}
                disabled={cleanupPreview.candidates.length === 0}
              >
                {busyAction?.startsWith("cleanup-") ? "清理中..." : "确认清理备份"}
              </button>
            </div>
          </div>
        </div>
      ) : null}
    </main>
  );
}

function inferTargetFileName(draft: RuleDraft) {
  return getPathBaseName(draft.sourcePath) || getPathBaseName(draft.targetPath) || "README.md";
}

function getPathBaseName(path: string) {
  const normalized = path.trim().replace(/[\\/]+$/, "");
  if (!normalized) {
    return "";
  }
  const segments = normalized.split(/[\\/]/);
  return segments[segments.length - 1] || "";
}

function getPathDirName(path: string) {
  const normalized = path.trim().replace(/[\\/]+$/, "");
  if (!normalized) {
    return "";
  }
  const separatorIndex = Math.max(normalized.lastIndexOf("\\"), normalized.lastIndexOf("/"));
  if (separatorIndex <= 0) {
    return "";
  }
  return normalized.slice(0, separatorIndex);
}

function describeFileTargetPath(path: string) {
  const fileName = getPathBaseName(path);
  const directory = getPathDirName(path);
  const extensionIndex = fileName.lastIndexOf(".");
  return {
    fileName,
    directory,
    hasExtension: extensionIndex > 0 && extensionIndex < fileName.length - 1,
  };
}

function joinPath(basePath: string, childName: string) {
  const trimmedBase = basePath.replace(/[\\/]+$/, "");
  if (!trimmedBase) {
    return childName;
  }
  const separator = trimmedBase.includes("\\") ? "\\" : "/";
  return `${trimmedBase}${separator}${childName}`;
}

function sanitizeDraft(draft: RuleDraft): RuleDraft {
  return {
    ...draft,
    name: draft.name.trim(),
    sourcePath: draft.sourcePath.trim(),
    targetPath: draft.targetPath.trim(),
    includeGlobs: draft.includeGlobs.filter(Boolean),
    excludeGlobs: draft.excludeGlobs.filter(Boolean),
  };
}

function splitLines(value: string) {
  return value
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter(Boolean);
}

function formatDateTime(value: string | null | undefined) {
  if (!value) {
    return "尚无记录";
  }

  const parsed = new Date(value);
  if (Number.isNaN(parsed.getTime())) {
    return value;
  }

  return new Intl.DateTimeFormat("zh-CN", {
    hour12: false,
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  }).format(parsed);
}

function triggerLabel(trigger: string) {
  switch (trigger) {
    case "manual":
      return "手动";
    case "watch":
      return "监听";
    case "poll":
      return "轮询";
    case "cleanup":
      return "备份清理";
    case "startup":
      return "启动";
    default:
      return trigger;
  }
}

function healthLabel(health: string) {
  switch (health) {
    case "healthy":
      return "健康";
    case "missingSource":
      return "源不存在";
    case "invalidSourceType":
      return "源类型错误";
    case "invalidTargetPath":
      return "目标路径无效";
    case "overlappingDirectories":
      return "目录互相包含";
    default:
      return health;
  }
}

function healthClassName(health: string) {
  return health === "healthy" ? "success" : "error";
}

function formatError(error: unknown) {
  if (typeof error === "string") {
    return error;
  }
  if (error instanceof Error) {
    return error.message;
  }
  return "发生未知错误。";
}

async function safeGetAutostartEnabled() {
  try {
    return await isAutostartEnabled();
  } catch {
    return false;
  }
}

async function ensureNotificationPermission() {
  let granted = await isPermissionGranted();
  if (!granted) {
    granted = (await requestPermission()) === "granted";
  }
  return granted;
}

async function maybeNotify(title: string, body: string) {
  const granted = await ensureNotificationPermission();
  if (granted) {
    sendNotification({ title, body });
  }
}

export default App;
