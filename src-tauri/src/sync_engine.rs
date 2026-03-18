use crate::models::{
    AppSettings, CleanupCandidate, CleanupCandidateKind, CleanupPreview, ConflictPolicy,
    DeletePolicy, RuleDraft, RuleHealth, RuleKind, RuleLastResult, SyncHistoryItem, SyncRule,
    SyncTrigger,
};
use blake3::Hasher;
use chrono::{DateTime, Duration, Utc};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::fs;
use std::io;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;
use walkdir::WalkDir;

#[derive(Debug, Clone, Default)]
pub struct RunCounts {
    pub copied_count: usize,
    pub updated_count: usize,
    pub skipped_count: usize,
    pub backup_count: usize,
    pub deleted_count: usize,
    pub error_count: usize,
}

#[derive(Debug, Clone)]
pub struct RuleRunOutcome {
    pub last_result: RuleLastResult,
    pub history_item: SyncHistoryItem,
}

struct Matcher {
    include: GlobSet,
    exclude: GlobSet,
}

impl Matcher {
    fn matches(&self, path: &Path) -> bool {
        self.include.is_match(path) && !self.exclude.is_match(path)
    }
}

pub fn build_rule(
    rule_id: Option<String>,
    draft: RuleDraft,
    settings: &AppSettings,
) -> Result<SyncRule, String> {
    let source_path = normalize_path(&draft.source_path)?;
    let target_path = normalize_target_path(&draft.target_path)?;

    validate_rule_paths(&draft.kind, &source_path, &target_path)?;

    Ok(SyncRule {
        id: rule_id.unwrap_or_else(|| Uuid::new_v4().to_string()),
        name: draft.name.trim().to_string(),
        enabled: draft.enabled,
        kind: draft.kind,
        source_path,
        target_path,
        auto_sync: draft.auto_sync,
        watch_enabled: draft.watch_enabled,
        poll_fallback_enabled: draft.poll_fallback_enabled,
        poll_interval_sec: draft.poll_interval_sec.max(5).max(
            settings
                .default_poll_interval_sec
                .min(draft.poll_interval_sec.max(5)),
        ),
        conflict_policy: ConflictPolicy::OverwriteWithBackup,
        delete_policy: DeletePolicy::NoDelete,
        include_globs: sanitize_globs(draft.include_globs),
        exclude_globs: sanitize_globs(draft.exclude_globs),
        last_sync_at: None,
        last_result: RuleLastResult::default(),
        health: RuleHealth::Healthy,
    })
}

pub fn evaluate_rule_health(rule: &SyncRule) -> RuleHealth {
    let source = PathBuf::from(&rule.source_path);
    let target = PathBuf::from(&rule.target_path);

    if !source.exists() {
        return RuleHealth::MissingSource;
    }

    match rule.kind {
        RuleKind::File if !source.is_file() => return RuleHealth::InvalidSourceType,
        RuleKind::Folder if !source.is_dir() => return RuleHealth::InvalidSourceType,
        _ => {}
    }

    if target.exists() {
        match rule.kind {
            RuleKind::File if target.is_dir() => return RuleHealth::InvalidTargetPath,
            RuleKind::Folder if target.is_file() => return RuleHealth::InvalidTargetPath,
            _ => {}
        }
    }

    if !target.is_absolute() {
        return RuleHealth::InvalidTargetPath;
    }

    if matches!(rule.kind, RuleKind::Folder) && paths_overlap(&source, &target) {
        return RuleHealth::OverlappingDirectories;
    }

    RuleHealth::Healthy
}

pub fn run_rule_sync(
    rule: &SyncRule,
    settings: &AppSettings,
    trigger: SyncTrigger,
) -> RuleRunOutcome {
    let started_at = Utc::now();
    match sync_rule_files(rule, settings) {
        Ok(counts) => build_outcome(
            rule,
            trigger,
            started_at,
            true,
            success_message(&counts),
            counts,
        ),
        Err(error) => {
            let counts = RunCounts {
                error_count: 1,
                ..RunCounts::default()
            };
            build_outcome(rule, trigger, started_at, false, error, counts)
        }
    }
}

pub fn preview_rule_cleanup(rule: &SyncRule) -> Result<CleanupPreview, String> {
    let candidates = backup_cleanup_candidates(rule)?;
    let file_count = candidates
        .iter()
        .filter(|candidate| matches!(candidate.kind, CleanupCandidateKind::File))
        .count();
    let folder_count = candidates
        .iter()
        .filter(|candidate| matches!(candidate.kind, CleanupCandidateKind::Folder))
        .count();

    Ok(CleanupPreview {
        rule_id: rule.id.clone(),
        rule_name: rule.name.clone(),
        candidates,
        file_count,
        folder_count,
    })
}

pub fn execute_rule_cleanup(rule: &SyncRule) -> Result<RuleRunOutcome, String> {
    let preview = preview_rule_cleanup(rule)?;
    let started_at = Utc::now();
    let mut counts = RunCounts::default();

    let mut candidates = preview.candidates;
    candidates.sort_by(|left, right| right.path.len().cmp(&left.path.len()));

    for candidate in candidates {
        let path = PathBuf::from(&candidate.path);
        if !path.exists() {
            continue;
        }

        match candidate.kind {
            CleanupCandidateKind::File => {
                fs::remove_file(&path).map_err(to_error)?;
                counts.deleted_count += 1;
            }
            CleanupCandidateKind::Folder => {
                fs::remove_dir_all(&path).map_err(to_error)?;
                counts.deleted_count += 1;
            }
        }
    }

    Ok(build_outcome(
        rule,
        SyncTrigger::Cleanup,
        started_at,
        true,
        if counts.deleted_count == 0 {
            "没有需要清理的备份。".to_string()
        } else {
            format!("备份清理完成：删除 {} 个备份文件或目录。", counts.deleted_count)
        },
        counts,
    ))
}

fn sync_rule_files(rule: &SyncRule, settings: &AppSettings) -> Result<RunCounts, String> {
    match rule.kind {
        RuleKind::File => sync_single_file(rule, settings),
        RuleKind::Folder => sync_folder(rule, settings),
    }
}

fn sync_single_file(rule: &SyncRule, settings: &AppSettings) -> Result<RunCounts, String> {
    let source = PathBuf::from(&rule.source_path);
    let target = PathBuf::from(&rule.target_path);
    let mut counts = RunCounts::default();

    ensure_parent_dir(&target)?;

    if !target.exists() {
        copy_file_atomic(&source, &target)?;
        counts.copied_count += 1;
    } else if files_match(&source, &target)? {
        counts.skipped_count += 1;
    } else {
        backup_target_file(rule, &target, None)?;
        purge_expired_backups(rule, settings.backup_retention_days)?;
        copy_file_atomic(&source, &target)?;
        counts.updated_count += 1;
        counts.backup_count += 1;
    }

    purge_expired_backups(rule, settings.backup_retention_days)?;
    Ok(counts)
}

fn sync_folder(rule: &SyncRule, settings: &AppSettings) -> Result<RunCounts, String> {
    let source_root = PathBuf::from(&rule.source_path);
    let target_root = PathBuf::from(&rule.target_path);
    let matcher = build_matcher(rule)?;
    let mut counts = RunCounts::default();

    fs::create_dir_all(&target_root).map_err(to_error)?;

    for entry in WalkDir::new(&source_root)
        .into_iter()
        .filter_map(Result::ok)
    {
        let source_path = entry.path();
        if entry.file_type().is_dir() {
            continue;
        }

        let relative_path = source_path.strip_prefix(&source_root).map_err(to_error)?;
        if !matcher.matches(relative_path) {
            continue;
        }

        let target_path = target_root.join(relative_path);
        ensure_parent_dir(&target_path)?;

        if !target_path.exists() {
            copy_file_atomic(source_path, &target_path)?;
            counts.copied_count += 1;
            continue;
        }

        if files_match(source_path, &target_path)? {
            counts.skipped_count += 1;
            continue;
        }

        backup_target_file(rule, &target_path, Some(relative_path))?;
        copy_file_atomic(source_path, &target_path)?;
        counts.updated_count += 1;
        counts.backup_count += 1;
    }

    purge_expired_backups(rule, settings.backup_retention_days)?;
    Ok(counts)
}

fn build_matcher(rule: &SyncRule) -> Result<Matcher, String> {
    let mut include_builder = GlobSetBuilder::new();
    if rule.include_globs.is_empty() {
        include_builder.add(Glob::new("**/*").map_err(to_error)?);
    } else {
        for pattern in &rule.include_globs {
            include_builder.add(Glob::new(pattern).map_err(to_error)?);
        }
    }

    let mut exclude_builder = GlobSetBuilder::new();
    for pattern in &rule.exclude_globs {
        exclude_builder.add(Glob::new(pattern).map_err(to_error)?);
    }

    Ok(Matcher {
        include: include_builder.build().map_err(to_error)?,
        exclude: exclude_builder.build().map_err(to_error)?,
    })
}

fn backup_target_file(
    rule: &SyncRule,
    target_file: &Path,
    relative: Option<&Path>,
) -> Result<(), String> {
    if !target_file.exists() {
        return Ok(());
    }

    let backup_root = backup_root(rule)?;
    let backup_path = match rule.kind {
        RuleKind::File => {
            fs::create_dir_all(&backup_root).map_err(to_error)?;
            timestamped_file_name(&backup_root, target_file.file_name().unwrap_or_default())
        }
        RuleKind::Folder => {
            let relative = relative.ok_or_else(|| "缺少目录规则的相对路径。".to_string())?;
            let target_base = Path::new(&rule.target_path)
                .file_name()
                .ok_or_else(|| "目标目录无效，无法生成备份。".to_string())?;
            let base_dir = backup_root.join(target_base);
            let path_inside_backup = base_dir.join(relative);
            if let Some(parent) = path_inside_backup.parent() {
                fs::create_dir_all(parent).map_err(to_error)?;
            }
            timestamped_path(&path_inside_backup)
        }
    };

    fs::copy(target_file, backup_path).map_err(to_error)?;
    Ok(())
}

fn purge_expired_backups(rule: &SyncRule, retention_days: u64) -> Result<(), String> {
    if retention_days == 0 {
        return Ok(());
    }

    let cutoff = Utc::now() - Duration::days(retention_days as i64);
    let backup_root = backup_root(rule)?;
    if !backup_root.exists() {
        return Ok(());
    }

    for entry in WalkDir::new(&backup_root)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }

        let metadata = entry.metadata().map_err(to_error)?;
        let modified: DateTime<Utc> = metadata
            .modified()
            .map(DateTime::<Utc>::from)
            .map_err(to_error)?;
        if modified < cutoff {
            fs::remove_file(entry.path()).map_err(to_error)?;
        }
    }

    remove_empty_dirs(&backup_root)?;
    Ok(())
}

fn remove_empty_dirs(root: &Path) -> Result<(), String> {
    let mut directories = WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_dir())
        .map(|entry| entry.into_path())
        .collect::<Vec<_>>();

    directories.sort_by(|left, right| right.components().count().cmp(&left.components().count()));

    for directory in directories {
        if directory == root {
            continue;
        }
        let is_empty = fs::read_dir(&directory).map_err(to_error)?.next().is_none();
        if is_empty {
            fs::remove_dir(&directory).map_err(to_error)?;
        }
    }

    Ok(())
}

fn files_match(source: &Path, target: &Path) -> Result<bool, String> {
    let source_meta = fs::metadata(source).map_err(to_error)?;
    let target_meta = fs::metadata(target).map_err(to_error)?;

    if source_meta.len() != target_meta.len() {
        return Ok(false);
    }

    if source_meta.modified().map_err(to_error)? == target_meta.modified().map_err(to_error)? {
        return Ok(true);
    }

    Ok(hash_file(source)? == hash_file(target)?)
}

fn hash_file(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(to_error)?;
    let mut hasher = Hasher::new();
    let mut buffer = [0_u8; 8192];

    loop {
        let read = file.read(&mut buffer).map_err(to_error)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(hasher.finalize().to_hex().to_string())
}

fn copy_file_atomic(source: &Path, target: &Path) -> Result<(), String> {
    ensure_parent_dir(target)?;
    let temp_path = target.with_file_name(format!(
        ".{}.{}.filesync.tmp",
        target
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("temp"),
        Uuid::new_v4()
    ));

    {
        let mut source_file = fs::File::open(source)
            .map_err(|error| format_io_error("读取源文件失败", source, error))?;
        let mut temp_file = fs::File::create(&temp_path)
            .map_err(|error| format_io_error("创建临时文件失败", &temp_path, error))?;
        std::io::copy(&mut source_file, &mut temp_file)
            .map_err(|error| format_io_error("写入临时文件失败", &temp_path, error))?;
        temp_file
            .flush()
            .map_err(|error| format_io_error("刷新临时文件失败", &temp_path, error))?;
    }

    if target.exists() {
        let _ = clear_readonly(target);
    }

    if let Err(first_rename_error) = fs::rename(&temp_path, target) {
        if target.exists() {
            clear_readonly(target)
                .map_err(|error| format_io_error("移除只读属性失败", target, error))?;
            fs::remove_file(target)
                .map_err(|error| format_io_error("删除旧目标文件失败", target, error))?;
        }

        if let Err(second_rename_error) = fs::rename(&temp_path, target) {
            let _ = clear_readonly(target);
            fs::copy(&temp_path, target).map_err(|copy_error| {
                format_copy_fallback_error(
                    target,
                    first_rename_error,
                    second_rename_error,
                    copy_error,
                )
            })?;
            fs::remove_file(&temp_path)
                .map_err(|error| format_io_error("清理临时文件失败", &temp_path, error))?;
        }
    }

    Ok(())
}

fn ensure_parent_dir(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "目标路径缺少父目录。".to_string())?;
    fs::create_dir_all(parent).map_err(|error| format_io_error("创建目标目录失败", parent, error))
}

fn normalize_path(input: &str) -> Result<String, String> {
    let trimmed = input.trim().trim_matches('"');
    if trimmed.is_empty() {
        return Err("路径不能为空。".to_string());
    }

    let path = PathBuf::from(trimmed);
    if !path.exists() {
        return Err("源路径不存在。".to_string());
    }
    if !path.is_absolute() {
        return Err("请使用绝对路径。".to_string());
    }

    Ok(path.to_string_lossy().to_string())
}

fn normalize_target_path(input: &str) -> Result<String, String> {
    let trimmed = input.trim().trim_matches('"');
    if trimmed.is_empty() {
        return Err("目标路径不能为空。".to_string());
    }
    let path = PathBuf::from(trimmed);
    if !path.is_absolute() {
        return Err("目标路径必须是绝对路径。".to_string());
    }
    Ok(path.to_string_lossy().to_string())
}

fn validate_rule_paths(kind: &RuleKind, source: &str, target: &str) -> Result<(), String> {
    let source_path = PathBuf::from(source);
    let target_path = PathBuf::from(target);

    match kind {
        RuleKind::File if !source_path.is_file() => {
            return Err("文件规则要求源路径必须是文件。".to_string())
        }
        RuleKind::Folder if !source_path.is_dir() => {
            return Err("文件夹规则要求源路径必须是文件夹。".to_string())
        }
        _ => {}
    }

    if target_path.exists() {
        match kind {
            RuleKind::File if target_path.is_dir() => {
                return Err("文件规则的目标路径不能是文件夹，请选择最终文件路径。".to_string())
            }
            RuleKind::Folder if target_path.is_file() => {
                return Err("文件夹规则的目标路径不能是文件，请选择目标目录路径。".to_string())
            }
            _ => {}
        }
    }

    if source_path == target_path {
        return Err("源路径和目标路径不能相同。".to_string());
    }

    if matches!(kind, RuleKind::Folder) && paths_overlap(&source_path, &target_path) {
        return Err("文件夹规则中，源目录和目标目录不能互相包含。".to_string());
    }

    Ok(())
}

fn paths_overlap(source: &Path, target: &Path) -> bool {
    let source_components = normalized_components(source);
    let target_components = normalized_components(target);
    source_components.starts_with(&target_components)
        || target_components.starts_with(&source_components)
}

fn normalized_components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().to_lowercase()),
            Component::Prefix(value) => Some(value.as_os_str().to_string_lossy().to_lowercase()),
            Component::RootDir => Some(std::path::MAIN_SEPARATOR.to_string()),
            _ => None,
        })
        .collect()
}

fn sanitize_globs(patterns: Vec<String>) -> Vec<String> {
    patterns
        .into_iter()
        .map(|pattern| pattern.trim().to_string())
        .filter(|pattern| !pattern.is_empty())
        .collect()
}

fn normalize_relative(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn timestamped_file_name(root: &Path, original_name: &std::ffi::OsStr) -> PathBuf {
    let original = PathBuf::from(original_name);
    timestamped_path(&root.join(original))
}

fn timestamped_path(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("backup");
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    let timestamp = Utc::now().format("%Y%m%d-%H%M%S");
    let file_name = if extension.is_empty() {
        format!("{stem}--{timestamp}")
    } else {
        format!("{stem}--{timestamp}.{extension}")
    };

    path.with_file_name(file_name)
}

fn backup_root(rule: &SyncRule) -> Result<PathBuf, String> {
    let target = PathBuf::from(&rule.target_path);
    let parent = target
        .parent()
        .ok_or_else(|| "目标路径缺少父目录，无法生成备份目录。".to_string())?;

    Ok(parent.join(".back"))
}

fn backup_cleanup_candidates(rule: &SyncRule) -> Result<Vec<CleanupCandidate>, String> {
    match rule.kind {
        RuleKind::File => backup_candidates_for_file_rule(rule),
        RuleKind::Folder => backup_candidates_for_folder_rule(rule),
    }
}

fn backup_candidates_for_file_rule(rule: &SyncRule) -> Result<Vec<CleanupCandidate>, String> {
    let root = backup_root(rule)?;
    if !root.exists() {
        return Ok(Vec::new());
    }

    let target = PathBuf::from(&rule.target_path);
    let original_name = target
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "目标文件名无效，无法预览备份。".to_string())?;
    let stem = target
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("backup");
    let extension = target.extension().and_then(|value| value.to_str()).unwrap_or("");

    let mut candidates = Vec::new();
    for entry in fs::read_dir(&root).map_err(to_error)? {
        let entry = entry.map_err(to_error)?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let file_name = path.file_name().and_then(|value| value.to_str()).unwrap_or("");
        let is_match = if extension.is_empty() {
            file_name.starts_with(&format!("{stem}--"))
        } else {
            file_name.starts_with(&format!("{stem}--")) && file_name.ends_with(&format!(".{extension}"))
        };

        if is_match {
            candidates.push(CleanupCandidate {
                path: path.to_string_lossy().to_string(),
                relative_path: original_name.to_string(),
                kind: CleanupCandidateKind::File,
            });
        }
    }

    candidates.sort_by(|left, right| right.path.cmp(&left.path));
    Ok(candidates)
}

fn backup_candidates_for_folder_rule(rule: &SyncRule) -> Result<Vec<CleanupCandidate>, String> {
    let scope = backup_root(rule)?.join(
        Path::new(&rule.target_path)
            .file_name()
            .ok_or_else(|| "目标目录无效，无法预览备份。".to_string())?,
    );
    if !scope.exists() {
        return Ok(Vec::new());
    }

    let mut candidates = Vec::new();
    for entry in WalkDir::new(&scope).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        if path == scope {
            continue;
        }

        candidates.push(CleanupCandidate {
            path: path.to_string_lossy().to_string(),
            relative_path: normalize_relative(path.strip_prefix(&scope).map_err(to_error)?),
            kind: if entry.file_type().is_dir() {
                CleanupCandidateKind::Folder
            } else {
                CleanupCandidateKind::File
            },
        });
    }

    candidates.sort_by(|left, right| right.path.len().cmp(&left.path.len()));
    Ok(candidates)
}

fn build_outcome(
    rule: &SyncRule,
    trigger: SyncTrigger,
    started_at: DateTime<Utc>,
    success: bool,
    message: String,
    counts: RunCounts,
) -> RuleRunOutcome {
    let finished_at = Utc::now();
    let last_result = RuleLastResult {
        success,
        message: message.clone(),
        trigger: Some(trigger.clone()),
        started_at: Some(started_at.to_rfc3339()),
        finished_at: Some(finished_at.to_rfc3339()),
        copied_count: counts.copied_count,
        updated_count: counts.updated_count,
        skipped_count: counts.skipped_count,
        backup_count: counts.backup_count,
        deleted_count: counts.deleted_count,
        error_count: counts.error_count,
    };

    let history_item = SyncHistoryItem {
        rule_id: rule.id.clone(),
        rule_name: rule.name.clone(),
        started_at: started_at.to_rfc3339(),
        finished_at: finished_at.to_rfc3339(),
        trigger,
        copied_count: counts.copied_count,
        updated_count: counts.updated_count,
        skipped_count: counts.skipped_count,
        backup_count: counts.backup_count,
        deleted_count: counts.deleted_count,
        error_count: counts.error_count,
        success,
        message,
    };

    RuleRunOutcome {
        last_result,
        history_item,
    }
}

fn success_message(counts: &RunCounts) -> String {
    if counts.copied_count == 0
        && counts.updated_count == 0
        && counts.skipped_count > 0
        && counts.backup_count == 0
    {
        return format!(
            "未执行覆盖：目标文件已存在且内容一致（未变更 {}）。",
            counts.skipped_count
        );
    }

    format!(
        "同步完成：新增 {}，更新 {}，未变更 {}，备份 {}。",
        counts.copied_count, counts.updated_count, counts.skipped_count, counts.backup_count
    )
}

fn to_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn clear_readonly(path: &Path) -> io::Result<()> {
    let metadata = fs::metadata(path)?;
    let mut permissions = metadata.permissions();
    if permissions.readonly() {
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

fn format_io_error(action: &str, path: &Path, error: io::Error) -> String {
    let mut message = format!("{action}：{}\n{}", path.display(), error);
    if error.raw_os_error() == Some(5) {
        message.push_str(
            "\n可能原因：目标文件是只读的、正在被 Unreal Editor / 编辑器占用，或当前程序没有写入权限。",
        );
    }
    message
}

fn format_copy_fallback_error(
    target: &Path,
    first_rename_error: io::Error,
    second_rename_error: io::Error,
    copy_error: io::Error,
) -> String {
    let mut message = format!(
        "写入目标文件失败：{}\n首次替换失败：{}\n二次替换失败：{}\n最终复制失败：{}",
        target.display(),
        first_rename_error,
        second_rename_error,
        copy_error
    );
    if copy_error.raw_os_error() == Some(5)
        || first_rename_error.raw_os_error() == Some(5)
        || second_rename_error.raw_os_error() == Some(5)
    {
        message.push_str(
            "\n可能原因：目标文件是只读的、正在被 Unreal Editor / 编辑器占用，或当前程序没有写入权限。",
        );
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AppSettings, RuleDraft, RuleKind};
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn create_rule_allows_missing_target() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("note.md");
        fs::write(&source, "hello").unwrap();
        let target = temp.path().join("project").join("README.md");
        let settings = AppSettings::default();

        let draft = RuleDraft {
            name: "README".into(),
            enabled: true,
            kind: RuleKind::File,
            source_path: source.to_string_lossy().to_string(),
            target_path: target.to_string_lossy().to_string(),
            auto_sync: true,
            watch_enabled: true,
            poll_fallback_enabled: true,
            poll_interval_sec: 300,
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
        };

        let rule = build_rule(None, draft, &settings).unwrap();
        assert_eq!(rule.target_path, target.to_string_lossy());
    }

    #[test]
    fn first_sync_creates_target_without_backup() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("note.md");
        let target = temp.path().join("project").join("README.md");
        fs::write(&source, "hello").unwrap();

        let settings = AppSettings::default();
        let draft = RuleDraft {
            name: "README".into(),
            enabled: true,
            kind: RuleKind::File,
            source_path: source.to_string_lossy().to_string(),
            target_path: target.to_string_lossy().to_string(),
            auto_sync: true,
            watch_enabled: true,
            poll_fallback_enabled: true,
            poll_interval_sec: 300,
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
        };
        let rule = build_rule(None, draft, &settings).unwrap();

        let outcome = run_rule_sync(&rule, &settings, SyncTrigger::Manual);
        assert!(target.exists());
        assert_eq!(fs::read_to_string(&target).unwrap(), "hello");
        assert_eq!(outcome.last_result.backup_count, 0);
    }

    #[test]
    fn unchanged_file_does_not_create_backup() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("note.md");
        let target = temp.path().join("project").join("README.md");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&source, "same-content").unwrap();
        fs::write(&target, "same-content").unwrap();

        let settings = AppSettings::default();
        let draft = RuleDraft {
            name: "README".into(),
            enabled: true,
            kind: RuleKind::File,
            source_path: source.to_string_lossy().to_string(),
            target_path: target.to_string_lossy().to_string(),
            auto_sync: true,
            watch_enabled: true,
            poll_fallback_enabled: true,
            poll_interval_sec: 300,
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
        };
        let rule = build_rule(None, draft, &settings).unwrap();

        let outcome = run_rule_sync(&rule, &settings, SyncTrigger::Manual);
        assert_eq!(outcome.last_result.backup_count, 0);
        assert_eq!(outcome.last_result.skipped_count, 1);
        assert!(!backup_root(&rule).unwrap().exists());
    }

    #[test]
    fn changed_file_creates_backup_once() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("note.md");
        let target = temp.path().join("project").join("README.md");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&source, "new-content").unwrap();
        fs::write(&target, "old-content").unwrap();

        let settings = AppSettings::default();
        let draft = RuleDraft {
            name: "README".into(),
            enabled: true,
            kind: RuleKind::File,
            source_path: source.to_string_lossy().to_string(),
            target_path: target.to_string_lossy().to_string(),
            auto_sync: true,
            watch_enabled: true,
            poll_fallback_enabled: true,
            poll_interval_sec: 300,
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
        };
        let rule = build_rule(None, draft, &settings).unwrap();

        let outcome = run_rule_sync(&rule, &settings, SyncTrigger::Manual);
        let backup_files = WalkDir::new(backup_root(&rule).unwrap())
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .count();

        assert_eq!(outcome.last_result.backup_count, 1);
        assert_eq!(outcome.last_result.updated_count, 1);
        assert_eq!(fs::read_to_string(&target).unwrap(), "new-content");
        assert_eq!(backup_files, 1);
    }

    #[test]
    fn unchanged_folder_file_does_not_create_backup() {
        let temp = tempdir().unwrap();
        let source_root = temp.path().join("notes");
        let target_root = temp.path().join("docs");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&target_root).unwrap();
        fs::write(source_root.join("guide.md"), "same-folder-content").unwrap();
        fs::write(target_root.join("guide.md"), "same-folder-content").unwrap();

        let settings = AppSettings::default();
        let draft = RuleDraft {
            name: "docs".into(),
            enabled: true,
            kind: RuleKind::Folder,
            source_path: source_root.to_string_lossy().to_string(),
            target_path: target_root.to_string_lossy().to_string(),
            auto_sync: true,
            watch_enabled: true,
            poll_fallback_enabled: true,
            poll_interval_sec: 300,
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
        };
        let rule = build_rule(None, draft, &settings).unwrap();

        let outcome = run_rule_sync(&rule, &settings, SyncTrigger::Manual);
        assert_eq!(outcome.last_result.backup_count, 0);
        assert_eq!(outcome.last_result.skipped_count, 1);
        assert!(!backup_root(&rule).unwrap().exists());
    }

    #[test]
    fn file_rule_rejects_existing_directory_target() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("note.md");
        let target = temp.path().join("project");
        fs::write(&source, "hello").unwrap();
        fs::create_dir_all(&target).unwrap();

        let settings = AppSettings::default();
        let draft = RuleDraft {
            name: "README".into(),
            enabled: true,
            kind: RuleKind::File,
            source_path: source.to_string_lossy().to_string(),
            target_path: target.to_string_lossy().to_string(),
            auto_sync: true,
            watch_enabled: true,
            poll_fallback_enabled: true,
            poll_interval_sec: 300,
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
        };

        let error = build_rule(None, draft, &settings).unwrap_err();
        assert!(error.contains("目标路径不能是文件夹"));
    }

    #[test]
    fn folder_rule_rejects_existing_file_target() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("notes");
        let target = temp.path().join("README.md");
        fs::create_dir_all(&source).unwrap();
        fs::write(&target, "hello").unwrap();

        let settings = AppSettings::default();
        let draft = RuleDraft {
            name: "docs".into(),
            enabled: true,
            kind: RuleKind::Folder,
            source_path: source.to_string_lossy().to_string(),
            target_path: target.to_string_lossy().to_string(),
            auto_sync: true,
            watch_enabled: true,
            poll_fallback_enabled: true,
            poll_interval_sec: 300,
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
        };

        let error = build_rule(None, draft, &settings).unwrap_err();
        assert!(error.contains("目标路径不能是文件"));
    }
}
