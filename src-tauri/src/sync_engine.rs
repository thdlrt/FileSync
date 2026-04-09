use crate::models::{
    AppSettings, CleanupCandidate, CleanupCandidateKind, CleanupPreview, ConflictPolicy,
    DeletePolicy, RuleDraft, RuleHealth, RuleKind, RuleLastResult, RuleSyncState,
    SyncHistoryItem, SyncRule, SyncTrigger,
};
use blake3::Hasher;
use chrono::{DateTime, Duration, Utc};
use filetime::{set_file_mtime, FileTime};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::collections::{BTreeMap, BTreeSet};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuleSide {
    Source,
    Target,
}

impl RuleSide {
    fn opposite(self) -> Self {
        match self {
            Self::Source => Self::Target,
            Self::Target => Self::Source,
        }
    }
}

const FILE_SYNC_ENTRY: &str = "__file__";

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
    let source_path = normalize_source_path(&draft.source_path)?;
    let target_path = normalize_target_path(&draft.target_path)?;
    let delete_policy = default_delete_policy(draft.kind.clone(), draft.bidirectional);

    validate_rule_paths(&draft.kind, &source_path, &target_path, draft.bidirectional)?;

    Ok(SyncRule {
        id: rule_id.unwrap_or_else(|| Uuid::new_v4().to_string()),
        name: draft.name.trim().to_string(),
        enabled: draft.enabled,
        kind: draft.kind,
        source_path,
        target_path,
        bidirectional: draft.bidirectional,
        auto_sync: draft.auto_sync,
        watch_enabled: draft.watch_enabled,
        poll_fallback_enabled: draft.poll_fallback_enabled,
        poll_interval_sec: draft.poll_interval_sec.max(5).max(
            settings
                .default_poll_interval_sec
                .min(draft.poll_interval_sec.max(5)),
        ),
        conflict_policy: ConflictPolicy::OverwriteWithBackup,
        delete_policy,
        include_globs: sanitize_globs(draft.include_globs),
        exclude_globs: sanitize_globs(draft.exclude_globs),
        sync_state: RuleSyncState::default(),
        last_sync_at: None,
        last_result: RuleLastResult::default(),
        health: RuleHealth::Healthy,
        config_error: None,
    })
}

pub fn evaluate_rule_health(rule: &SyncRule) -> RuleHealth {
    if rule.config_error.is_some() {
        return RuleHealth::InvalidConfiguration;
    }

    let source = PathBuf::from(&rule.source_path);
    let target = PathBuf::from(&rule.target_path);

    if !source.is_absolute() || !target.is_absolute() {
        return RuleHealth::InvalidTargetPath;
    }

    if source == target {
        return RuleHealth::InvalidTargetPath;
    }

    let source_exists = source.exists();
    let target_exists = target.exists();

    if rule.bidirectional {
        if !source_exists && !target_exists {
            return RuleHealth::MissingSource;
        }

        match rule.kind {
            RuleKind::File => {
                if source_exists && !source.is_file() {
                    return RuleHealth::InvalidSourceType;
                }
                if target_exists && target.is_dir() {
                    return RuleHealth::InvalidTargetPath;
                }
            }
            RuleKind::Folder => {
                if source_exists && !source.is_dir() {
                    return RuleHealth::InvalidSourceType;
                }
                if target_exists && target.is_file() {
                    return RuleHealth::InvalidTargetPath;
                }
                if source_exists && target_exists && paths_overlap(&source, &target) {
                    return RuleHealth::OverlappingDirectories;
                }
            }
        }

        return RuleHealth::Healthy;
    }

    if !source_exists {
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
            success_message(rule, &counts),
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
            format!(
                "备份清理完成：删除 {} 个备份文件或目录。",
                counts.deleted_count
            )
        },
        counts,
    ))
}

fn sync_rule_files(rule: &SyncRule, settings: &AppSettings) -> Result<RunCounts, String> {
    match (rule.kind.clone(), rule.bidirectional) {
        (RuleKind::File, false) => sync_single_file(rule, settings),
        (RuleKind::Folder, false) => sync_folder(rule, settings),
        (RuleKind::File, true) => sync_bidirectional_file(rule, settings),
        (RuleKind::Folder, true) => sync_bidirectional_folder(rule, settings),
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
        backup_rule_file(rule, RuleSide::Target, &target, None)?;
        copy_file_atomic(&source, &target)?;
        counts.updated_count += 1;
        counts.backup_count += 1;
    }

    purge_expired_backups_for_rule(rule, settings.backup_retention_days)?;
    Ok(counts)
}

fn sync_folder(rule: &SyncRule, settings: &AppSettings) -> Result<RunCounts, String> {
    let source_root = PathBuf::from(&rule.source_path);
    let target_root = PathBuf::from(&rule.target_path);
    let matcher = build_matcher(rule)?;
    let source_files = collect_folder_files(&source_root, &matcher)?;
    let mut counts = RunCounts::default();

    fs::create_dir_all(&target_root).map_err(to_error)?;

    for (relative, source_path) in &source_files {
        let relative_path = Path::new(relative);
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

        backup_rule_file(rule, RuleSide::Target, &target_path, Some(relative_path))?;
        copy_file_atomic(source_path, &target_path)?;
        counts.updated_count += 1;
        counts.backup_count += 1;
    }

    if matches!(rule.delete_policy, DeletePolicy::MoveToBackup) {
        let target_files = collect_folder_files(&target_root, &matcher)?;
        for (relative, target_path) in target_files {
            if source_files.contains_key(&relative) {
                continue;
            }

            let relative_path = PathBuf::from(&relative);
            backup_rule_file(rule, RuleSide::Target, &target_path, Some(relative_path.as_path()))?;
            let _ = clear_readonly(&target_path);
            fs::remove_file(&target_path)
                .map_err(|error| format_io_error("删除目标侧多余文件失败", &target_path, error))?;
            counts.backup_count += 1;
            counts.deleted_count += 1;
        }

        remove_empty_dirs(&target_root)?;
    }

    purge_expired_backups_for_rule(rule, settings.backup_retention_days)?;
    Ok(counts)
}

fn sync_bidirectional_file(rule: &SyncRule, settings: &AppSettings) -> Result<RunCounts, String> {
    let source = PathBuf::from(&rule.source_path);
    let target = PathBuf::from(&rule.target_path);
    let mut counts = RunCounts::default();
    let was_synced = rule
        .sync_state
        .mirrored_entries
        .iter()
        .any(|entry| entry == FILE_SYNC_ENTRY);

    match (source.exists(), target.exists()) {
        (false, false) => {
            if was_synced {
                counts.skipped_count += 1;
            } else {
                return Err("双向同步失败：源路径和目标路径都不存在。".to_string());
            }
        }
        (true, false) => {
            if was_synced {
                delete_existing_file(rule, RuleSide::Source, &source, None, &mut counts)?;
            } else {
                copy_file_atomic(&source, &target)?;
                counts.copied_count += 1;
            }
        }
        (false, true) => {
            if was_synced {
                delete_existing_file(rule, RuleSide::Target, &target, None, &mut counts)?;
            } else {
                copy_file_atomic(&target, &source)?;
                counts.copied_count += 1;
            }
        }
        (true, true) => {
            if !source.is_file() || !target.is_file() {
                return Err("双向文件规则要求两侧都必须是文件。".to_string());
            }

            if files_match(&source, &target)? {
                counts.skipped_count += 1;
            } else {
                let winner = newer_side_by_timestamp(&source, &target)?;
                sync_file_pair(rule, winner, None, &mut counts)?;
            }
        }
    }

    purge_expired_backups_for_rule(rule, settings.backup_retention_days)?;
    Ok(counts)
}

fn sync_bidirectional_folder(rule: &SyncRule, settings: &AppSettings) -> Result<RunCounts, String> {
    let source_root = PathBuf::from(&rule.source_path);
    let target_root = PathBuf::from(&rule.target_path);
    let matcher = build_matcher(rule)?;
    let source_files = collect_folder_files(&source_root, &matcher)?;
    let target_files = collect_folder_files(&target_root, &matcher)?;
    let mut counts = RunCounts::default();
    let mirrored_entries = synced_entry_set(&rule.sync_state);

    let relative_paths = source_files
        .keys()
        .chain(target_files.keys())
        .chain(mirrored_entries.iter())
        .cloned()
        .collect::<BTreeSet<_>>();

    for relative in relative_paths {
        let relative_path = PathBuf::from(&relative);
        match (source_files.get(&relative), target_files.get(&relative)) {
            (Some(source_path), None) => {
                if mirrored_entries.contains(&relative) {
                    delete_existing_file(
                        rule,
                        RuleSide::Source,
                        source_path,
                        Some(relative_path.as_path()),
                        &mut counts,
                    )?;
                } else {
                    let target_path = target_root.join(&relative_path);
                    copy_file_atomic(source_path, &target_path)?;
                    counts.copied_count += 1;
                }
            }
            (None, Some(target_path)) => {
                if mirrored_entries.contains(&relative) {
                    delete_existing_file(
                        rule,
                        RuleSide::Target,
                        target_path,
                        Some(relative_path.as_path()),
                        &mut counts,
                    )?;
                } else {
                    let source_path = source_root.join(&relative_path);
                    copy_file_atomic(target_path, &source_path)?;
                    counts.copied_count += 1;
                }
            }
            (Some(source_path), Some(target_path)) => {
                if files_match(source_path, target_path)? {
                    counts.skipped_count += 1;
                    continue;
                }

                let winner = newer_side_by_timestamp(source_path, target_path)?;
                sync_file_pair(rule, winner, Some(relative_path.as_path()), &mut counts)?;
            }
            (None, None) => {
                counts.skipped_count += 1;
            }
        }
    }

    remove_empty_dirs(&source_root)?;
    remove_empty_dirs(&target_root)?;

    purge_expired_backups_for_rule(rule, settings.backup_retention_days)?;
    Ok(counts)
}

fn delete_existing_file(
    rule: &SyncRule,
    side: RuleSide,
    file_path: &Path,
    relative_path: Option<&Path>,
    counts: &mut RunCounts,
) -> Result<(), String> {
    backup_rule_file(rule, side, file_path, relative_path)?;
    let _ = clear_readonly(file_path);
    fs::remove_file(file_path)
        .map_err(|error| format_io_error("删除同步文件失败", file_path, error))?;
    counts.backup_count += 1;
    counts.deleted_count += 1;
    Ok(())
}

fn sync_file_pair(
    rule: &SyncRule,
    winner: RuleSide,
    relative_path: Option<&Path>,
    counts: &mut RunCounts,
) -> Result<(), String> {
    let source_path = match (winner, relative_path) {
        (RuleSide::Source, Some(relative)) => PathBuf::from(&rule.source_path).join(relative),
        (RuleSide::Target, Some(relative)) => PathBuf::from(&rule.target_path).join(relative),
        (RuleSide::Source, None) => PathBuf::from(&rule.source_path),
        (RuleSide::Target, None) => PathBuf::from(&rule.target_path),
    };
    let target_side = winner.opposite();
    let target_path = match (target_side, relative_path) {
        (RuleSide::Source, Some(relative)) => PathBuf::from(&rule.source_path).join(relative),
        (RuleSide::Target, Some(relative)) => PathBuf::from(&rule.target_path).join(relative),
        (RuleSide::Source, None) => PathBuf::from(&rule.source_path),
        (RuleSide::Target, None) => PathBuf::from(&rule.target_path),
    };

    if target_path.exists() {
        backup_rule_file(rule, target_side, &target_path, relative_path)?;
        counts.backup_count += 1;
        counts.updated_count += 1;
    } else {
        counts.copied_count += 1;
    }

    copy_file_atomic(&source_path, &target_path)
}

fn collect_folder_files(root: &Path, matcher: &Matcher) -> Result<BTreeMap<String, PathBuf>, String> {
    let mut files = BTreeMap::new();
    if !root.exists() {
        return Ok(files);
    }

    for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
        if entry.file_type().is_dir() {
            continue;
        }

        let relative_path = entry.path().strip_prefix(root).map_err(to_error)?;
        if !matcher.matches(relative_path) {
            continue;
        }

        files.insert(
            normalize_relative(relative_path),
            entry.path().to_path_buf(),
        );
    }

    Ok(files)
}

fn newer_side_by_timestamp(source: &Path, target: &Path) -> Result<RuleSide, String> {
    let source_modified = fs::metadata(source)
        .map_err(to_error)?
        .modified()
        .map_err(to_error)?;
    let target_modified = fs::metadata(target)
        .map_err(to_error)?
        .modified()
        .map_err(to_error)?;

    if target_modified > source_modified {
        Ok(RuleSide::Target)
    } else {
        Ok(RuleSide::Source)
    }
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

fn backup_rule_file(
    rule: &SyncRule,
    side: RuleSide,
    file_path: &Path,
    relative: Option<&Path>,
) -> Result<(), String> {
    if !file_path.exists() {
        return Ok(());
    }

    let backup_root = backup_root_for_side(rule, side)?;
    let backup_path = match rule.kind {
        RuleKind::File => {
            fs::create_dir_all(&backup_root).map_err(to_error)?;
            timestamped_file_name(&backup_root, file_path.file_name().unwrap_or_default())
        }
        RuleKind::Folder => {
            let relative = relative.ok_or_else(|| "缺少目录规则的相对路径。".to_string())?;
            let side_root = path_for_side(rule, side);
            let target_base = side_root
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

    fs::copy(file_path, backup_path).map_err(to_error)?;
    Ok(())
}

fn purge_expired_backups_for_rule(rule: &SyncRule, retention_days: u64) -> Result<(), String> {
    let mut roots = backup_sides(rule)
        .into_iter()
        .filter_map(|side| backup_root_for_side(rule, side).ok())
        .collect::<Vec<_>>();
    roots.sort();
    roots.dedup();

    for root in roots {
        purge_expired_backups(&root, retention_days)?;
    }

    Ok(())
}

fn purge_expired_backups(backup_root: &Path, retention_days: u64) -> Result<(), String> {
    if retention_days == 0 {
        return Ok(());
    }

    let cutoff = Utc::now() - Duration::days(retention_days as i64);
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

    preserve_modified_time(source, target)?;
    Ok(())
}

fn preserve_modified_time(source: &Path, target: &Path) -> Result<(), String> {
    let modified = fs::metadata(source)
        .map_err(|error| format_io_error("读取源文件时间失败", source, error))?
        .modified()
        .map_err(|error| format_io_error("读取源文件时间失败", source, error))?;
    set_file_mtime(target, FileTime::from_system_time(modified))
        .map_err(|error| format_io_error("写入目标文件时间失败", target, error))
}

fn ensure_parent_dir(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "目标路径缺少父目录。".to_string())?;
    fs::create_dir_all(parent).map_err(|error| format_io_error("创建目标目录失败", parent, error))
}

fn normalize_source_path(input: &str) -> Result<String, String> {
    let trimmed = input.trim().trim_matches('"');
    if trimmed.is_empty() {
        return Err("路径不能为空。".to_string());
    }

    let path = PathBuf::from(trimmed);
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

fn validate_rule_paths(
    kind: &RuleKind,
    source: &str,
    target: &str,
    bidirectional: bool,
) -> Result<(), String> {
    let source_path = PathBuf::from(source);
    let target_path = PathBuf::from(target);

    validate_existing_side(kind, &source_path, "源路径", true)?;
    validate_existing_side(kind, &target_path, "目标路径", false)?;

    if bidirectional {
        if !source_path.exists() && !target_path.exists() {
            return Err("双向同步要求源路径和目标路径至少有一侧已存在。".to_string());
        }
    } else if !source_path.exists() {
        return Err("源路径不存在。".to_string());
    }

    if source_path == target_path {
        return Err("源路径和目标路径不能相同。".to_string());
    }

    if matches!(kind, RuleKind::Folder) && paths_overlap(&source_path, &target_path) {
        return Err("文件夹规则中，源目录和目标目录不能互相包含。".to_string());
    }

    Ok(())
}

fn validate_existing_side(
    kind: &RuleKind,
    path: &Path,
    label: &str,
    is_source: bool,
) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }

    match kind {
        RuleKind::File if path.is_dir() => {
            if is_source {
                Err("文件规则要求源路径必须是文件。".to_string())
            } else {
                Err("文件规则的目标路径不能是文件夹，请选择最终文件路径。".to_string())
            }
        }
        RuleKind::Folder if path.is_file() => {
            if is_source {
                Err("文件夹规则要求源路径必须是文件夹。".to_string())
            } else {
                Err("文件夹规则的目标路径不能是文件，请选择目标目录路径。".to_string())
            }
        }
        _ => {
            let _ = label;
            Ok(())
        }
    }
}

pub(crate) fn default_delete_policy(kind: RuleKind, bidirectional: bool) -> DeletePolicy {
    let _ = bidirectional;
    if matches!(kind, RuleKind::Folder) {
        DeletePolicy::MoveToBackup
    } else {
        DeletePolicy::NoDelete
    }
}

pub(crate) fn capture_rule_sync_state(rule: &SyncRule) -> Result<RuleSyncState, String> {
    if !rule.bidirectional {
        return Ok(RuleSyncState::default());
    }

    match rule.kind {
        RuleKind::File => {
            let source = PathBuf::from(&rule.source_path);
            let target = PathBuf::from(&rule.target_path);
            if source.exists() && target.exists() && source.is_file() && target.is_file() && files_match(&source, &target)? {
                Ok(RuleSyncState {
                    mirrored_entries: vec![FILE_SYNC_ENTRY.to_string()],
                })
            } else {
                Ok(RuleSyncState::default())
            }
        }
        RuleKind::Folder => {
            let matcher = build_matcher(rule)?;
            let source_files = collect_folder_files(Path::new(&rule.source_path), &matcher)?;
            let target_files = collect_folder_files(Path::new(&rule.target_path), &matcher)?;
            let mirrored_entries = source_files
                .keys()
                .filter(|relative| target_files.contains_key(*relative))
                .cloned()
                .collect::<Vec<_>>();
            Ok(RuleSyncState { mirrored_entries })
        }
    }
}

fn synced_entry_set(sync_state: &RuleSyncState) -> BTreeSet<String> {
    sync_state.mirrored_entries.iter().cloned().collect()
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

#[cfg(test)]
fn backup_root(rule: &SyncRule) -> Result<PathBuf, String> {
    backup_root_for_side(rule, RuleSide::Target)
}

fn backup_root_for_side(rule: &SyncRule, side: RuleSide) -> Result<PathBuf, String> {
    let target = path_for_side(rule, side);
    let parent = target
        .parent()
        .ok_or_else(|| "目标路径缺少父目录，无法生成备份目录。".to_string())?;

    Ok(parent.join(".back"))
}

fn path_for_side(rule: &SyncRule, side: RuleSide) -> PathBuf {
    match side {
        RuleSide::Source => PathBuf::from(&rule.source_path),
        RuleSide::Target => PathBuf::from(&rule.target_path),
    }
}

fn backup_sides(rule: &SyncRule) -> Vec<RuleSide> {
    if rule.bidirectional {
        vec![RuleSide::Source, RuleSide::Target]
    } else {
        vec![RuleSide::Target]
    }
}

fn backup_cleanup_candidates(rule: &SyncRule) -> Result<Vec<CleanupCandidate>, String> {
    let mut candidates = Vec::new();
    for side in backup_sides(rule) {
        let mut side_candidates = match rule.kind {
            RuleKind::File => backup_candidates_for_file_rule(rule, side),
            RuleKind::Folder => backup_candidates_for_folder_rule(rule, side),
        }?;
        candidates.append(&mut side_candidates);
    }

    candidates.sort_by(|left, right| right.path.len().cmp(&left.path.len()));
    Ok(candidates)
}

fn backup_candidates_for_file_rule(rule: &SyncRule, side: RuleSide) -> Result<Vec<CleanupCandidate>, String> {
    let root = backup_root_for_side(rule, side)?;
    if !root.exists() {
        return Ok(Vec::new());
    }

    let target = path_for_side(rule, side);
    let original_name = target
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "目标文件名无效，无法预览备份。".to_string())?;
    let stem = target
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("backup");
    let extension = target
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("");

    let mut candidates = Vec::new();
    for entry in fs::read_dir(&root).map_err(to_error)? {
        let entry = entry.map_err(to_error)?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let file_name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        let is_match = if extension.is_empty() {
            file_name.starts_with(&format!("{stem}--"))
        } else {
            file_name.starts_with(&format!("{stem}--"))
                && file_name.ends_with(&format!(".{extension}"))
        };

        if is_match {
            candidates.push(CleanupCandidate {
                path: path.to_string_lossy().to_string(),
                relative_path: format_candidate_relative(rule.bidirectional, side, original_name),
                kind: CleanupCandidateKind::File,
            });
        }
    }

    Ok(candidates)
}

fn backup_candidates_for_folder_rule(rule: &SyncRule, side: RuleSide) -> Result<Vec<CleanupCandidate>, String> {
    let side_root = path_for_side(rule, side);
    let scope = backup_root_for_side(rule, side)?.join(
        side_root
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
            relative_path: format_candidate_relative(
                rule.bidirectional,
                side,
                &normalize_relative(path.strip_prefix(&scope).map_err(to_error)?),
            ),
            kind: if entry.file_type().is_dir() {
                CleanupCandidateKind::Folder
            } else {
                CleanupCandidateKind::File
            },
        });
    }

    Ok(candidates)
}

fn format_candidate_relative(bidirectional: bool, side: RuleSide, relative: &str) -> String {
    if bidirectional {
        format!("{} / {}", side_label(side), relative)
    } else {
        relative.to_string()
    }
}

fn side_label(side: RuleSide) -> &'static str {
    match side {
        RuleSide::Source => "源侧",
        RuleSide::Target => "目标侧",
    }
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

fn success_message(rule: &SyncRule, counts: &RunCounts) -> String {
    let mode = if rule.bidirectional { "双向同步" } else { "同步" };
    if counts.copied_count == 0
        && counts.updated_count == 0
        && counts.skipped_count > 0
        && counts.backup_count == 0
    {
        return format!(
            "{}完成：两侧内容已是最新状态（未变更 {}）。",
            mode,
            counts.skipped_count
        );
    }

    format!(
        "{}完成：新增 {}，更新 {}，删除 {}，未变更 {}，备份 {}。",
        mode,
        counts.copied_count,
        counts.updated_count,
        counts.deleted_count,
        counts.skipped_count,
        counts.backup_count
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
    use filetime::{set_file_mtime, FileTime};
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
            bidirectional: false,
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
            bidirectional: false,
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
            bidirectional: false,
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
            bidirectional: false,
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
            bidirectional: false,
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
    fn single_direction_folder_moves_extra_target_files_to_backup() {
        let temp = tempdir().unwrap();
        let source_root = temp.path().join("notes");
        let target_root = temp.path().join("docs");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(target_root.join("nested")).unwrap();
        fs::write(source_root.join("guide.md"), "source-guide").unwrap();
        fs::write(target_root.join("guide.md"), "source-guide").unwrap();
        fs::write(target_root.join("nested").join("old.md"), "legacy").unwrap();

        let settings = AppSettings::default();
        let draft = RuleDraft {
            name: "docs".into(),
            enabled: true,
            kind: RuleKind::Folder,
            source_path: source_root.to_string_lossy().to_string(),
            target_path: target_root.to_string_lossy().to_string(),
            bidirectional: false,
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

        assert!(!target_root.join("nested").join("old.md").exists());
        assert_eq!(outcome.last_result.deleted_count, 1);
        assert_eq!(outcome.last_result.backup_count, 1);
        assert_eq!(backup_files, 1);
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
            bidirectional: false,
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
            bidirectional: false,
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

    #[test]
    fn bidirectional_file_prefers_newer_timestamp() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("left.md");
        let target = temp.path().join("right.md");
        fs::write(&source, "old-source").unwrap();
        fs::write(&target, "new-target").unwrap();

        set_file_mtime(&source, FileTime::from_unix_time(1_700_000_000, 0)).unwrap();
        set_file_mtime(&target, FileTime::from_unix_time(1_700_000_100, 0)).unwrap();

        let settings = AppSettings::default();
        let draft = RuleDraft {
            name: "双向文件".into(),
            enabled: true,
            kind: RuleKind::File,
            source_path: source.to_string_lossy().to_string(),
            target_path: target.to_string_lossy().to_string(),
            bidirectional: true,
            auto_sync: true,
            watch_enabled: true,
            poll_fallback_enabled: true,
            poll_interval_sec: 300,
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
        };

        let rule = build_rule(None, draft, &settings).unwrap();
        let outcome = run_rule_sync(&rule, &settings, SyncTrigger::Manual);

        assert_eq!(fs::read_to_string(&source).unwrap(), "new-target");
        assert_eq!(fs::read_to_string(&target).unwrap(), "new-target");
        assert_eq!(outcome.last_result.updated_count, 1);
        assert_eq!(outcome.last_result.backup_count, 1);
    }

    #[test]
    fn bidirectional_folder_syncs_missing_and_newer_files() {
        let temp = tempdir().unwrap();
        let source_root = temp.path().join("notes");
        let target_root = temp.path().join("docs");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&target_root).unwrap();

        let source_only = source_root.join("source-only.md");
        let shared_source = source_root.join("shared.md");
        let target_only = target_root.join("target-only.md");
        let shared_target = target_root.join("shared.md");

        fs::write(&source_only, "from-source").unwrap();
        fs::write(&shared_source, "older-source").unwrap();
        fs::write(&target_only, "from-target").unwrap();
        fs::write(&shared_target, "newer-target").unwrap();

        set_file_mtime(&shared_source, FileTime::from_unix_time(1_700_000_000, 0)).unwrap();
        set_file_mtime(&shared_target, FileTime::from_unix_time(1_700_000_100, 0)).unwrap();

        let settings = AppSettings::default();
        let draft = RuleDraft {
            name: "双向目录".into(),
            enabled: true,
            kind: RuleKind::Folder,
            source_path: source_root.to_string_lossy().to_string(),
            target_path: target_root.to_string_lossy().to_string(),
            bidirectional: true,
            auto_sync: true,
            watch_enabled: true,
            poll_fallback_enabled: true,
            poll_interval_sec: 300,
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
        };

        let rule = build_rule(None, draft, &settings).unwrap();
        let outcome = run_rule_sync(&rule, &settings, SyncTrigger::Manual);

        assert_eq!(fs::read_to_string(source_root.join("target-only.md")).unwrap(), "from-target");
        assert_eq!(fs::read_to_string(target_root.join("source-only.md")).unwrap(), "from-source");
        assert_eq!(fs::read_to_string(source_root.join("shared.md")).unwrap(), "newer-target");
        assert_eq!(fs::read_to_string(target_root.join("shared.md")).unwrap(), "newer-target");
        assert_eq!(outcome.last_result.copied_count, 2);
        assert_eq!(outcome.last_result.updated_count, 1);
        assert_eq!(outcome.last_result.backup_count, 1);
    }

    #[test]
    fn bidirectional_file_propagates_deletion_after_baseline_sync() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("left.md");
        let target = temp.path().join("right.md");
        fs::write(&source, "shared-content").unwrap();
        fs::write(&target, "shared-content").unwrap();

        let settings = AppSettings::default();
        let draft = RuleDraft {
            name: "双向文件删除".into(),
            enabled: true,
            kind: RuleKind::File,
            source_path: source.to_string_lossy().to_string(),
            target_path: target.to_string_lossy().to_string(),
            bidirectional: true,
            auto_sync: true,
            watch_enabled: true,
            poll_fallback_enabled: true,
            poll_interval_sec: 300,
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
        };

        let mut rule = build_rule(None, draft, &settings).unwrap();
        let first = run_rule_sync(&rule, &settings, SyncTrigger::Manual);
        assert_eq!(first.last_result.skipped_count, 1);

        rule.sync_state = capture_rule_sync_state(&rule).unwrap();
        fs::remove_file(&source).unwrap();

        let outcome = run_rule_sync(&rule, &settings, SyncTrigger::Manual);
        let backup_files = WalkDir::new(backup_root_for_side(&rule, RuleSide::Target).unwrap())
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .count();

        assert!(!source.exists());
        assert!(!target.exists());
        assert_eq!(outcome.last_result.deleted_count, 1);
        assert_eq!(outcome.last_result.backup_count, 1);
        assert_eq!(backup_files, 1);
    }

    #[test]
    fn bidirectional_folder_propagates_deletion_after_baseline_sync() {
        let temp = tempdir().unwrap();
        let source_root = temp.path().join("notes");
        let target_root = temp.path().join("docs");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&target_root).unwrap();
        fs::write(source_root.join("shared.md"), "shared-content").unwrap();
        fs::write(target_root.join("shared.md"), "shared-content").unwrap();

        let settings = AppSettings::default();
        let draft = RuleDraft {
            name: "双向目录删除".into(),
            enabled: true,
            kind: RuleKind::Folder,
            source_path: source_root.to_string_lossy().to_string(),
            target_path: target_root.to_string_lossy().to_string(),
            bidirectional: true,
            auto_sync: true,
            watch_enabled: true,
            poll_fallback_enabled: true,
            poll_interval_sec: 300,
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
        };

        let mut rule = build_rule(None, draft, &settings).unwrap();
        let first = run_rule_sync(&rule, &settings, SyncTrigger::Manual);
        assert_eq!(first.last_result.skipped_count, 1);

        rule.sync_state = capture_rule_sync_state(&rule).unwrap();
        fs::remove_file(source_root.join("shared.md")).unwrap();

        let outcome = run_rule_sync(&rule, &settings, SyncTrigger::Manual);
        let backup_files = WalkDir::new(backup_root_for_side(&rule, RuleSide::Target).unwrap())
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .count();

        assert!(!source_root.join("shared.md").exists());
        assert!(!target_root.join("shared.md").exists());
        assert_eq!(outcome.last_result.deleted_count, 1);
        assert_eq!(outcome.last_result.backup_count, 1);
        assert_eq!(backup_files, 1);
    }
}
