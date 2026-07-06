use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::{
    error::AppError,
    repo_lock::RepoLock,
    skill_store::{ScenarioRecord, SkillStore, SkillTargetRecord},
    sync_engine, sync_metadata, tool_adapters,
    tool_service,
};

#[derive(Debug, Clone)]
pub struct ScenarioSyncTarget {
    pub skill_id: String,
    pub skill_name: String,
    pub tool: String,
    pub source: PathBuf,
    pub target: PathBuf,
    pub mode: sync_engine::SyncMode,
    /// Current content hash of the central skill source, copied from
    /// `SkillRecord.content_hash`. Compared against the previously
    /// synced `SkillTargetRecord.source_hash` to skip redundant
    /// Copy-mode resyncs at startup (issue #153).
    pub source_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncPreviewTarget {
    pub skill_id: String,
    pub skill_name: String,
    pub tool: String,
    pub target_path: String,
    pub mode: String,
}

pub fn ensure_scenario_exists(store: &SkillStore, scenario_id: &str) -> Result<(), AppError> {
    let exists = store
        .get_all_scenarios()
        .map_err(AppError::db)?
        .iter()
        .any(|s| s.id == scenario_id);
    if !exists {
        return Err(AppError::not_found("Scenario not found"));
    }
    Ok(())
}

pub fn enabled_installed_adapters_for_scenario_skill(
    store: &SkillStore,
    scenario_id: &str,
    skill_id: &str,
) -> Result<Vec<tool_adapters::ToolAdapter>, AppError> {
    let adapters = tool_adapters::enabled_installed_adapters(store);
    let adapter_keys: Vec<String> = adapters.iter().map(|a| a.key.clone()).collect();

    store
        .ensure_scenario_skill_tool_defaults(scenario_id, skill_id, &adapter_keys)
        .map_err(AppError::db)?;

    let enabled = store
        .get_enabled_tools_for_scenario_skill(scenario_id, skill_id)
        .map_err(AppError::db)?;
    let enabled_set: HashSet<String> = enabled.into_iter().collect();

    Ok(adapters
        .into_iter()
        .filter(|adapter| enabled_set.contains(&adapter.key))
        .collect())
}

pub fn collect_scenario_sync_targets(
    store: &SkillStore,
    scenario_id: &str,
) -> Result<Vec<ScenarioSyncTarget>, AppError> {
    let skills = store
        .get_skills_for_scenario(scenario_id)
        .map_err(AppError::db)?;
    let configured_mode = store.get_setting("sync_mode").map_err(AppError::db)?;
    let mut targets = Vec::new();

    for skill in &skills {
        let source = PathBuf::from(&skill.central_path);
        let target_name = sync_engine::target_dir_name(&source, &skill.name);
        let adapters = enabled_installed_adapters_for_scenario_skill(store, scenario_id, &skill.id)?;
        for adapter in &adapters {
            let target = adapter.skills_dir().join(&target_name);
            let mode = sync_engine::sync_mode_for_tool(&adapter.key, configured_mode.as_deref());
            targets.push(ScenarioSyncTarget {
                skill_id: skill.id.clone(),
                skill_name: skill.name.clone(),
                tool: adapter.key.clone(),
                source: source.clone(),
                target,
                mode,
                source_hash: skill.content_hash.clone(),
            });
        }
    }

    Ok(targets)
}

pub fn preview_scenario_sync(
    store: &SkillStore,
    scenario_id: &str,
) -> Result<Vec<SyncPreviewTarget>, AppError> {
    collect_scenario_sync_targets(store, scenario_id).map(|targets| {
        targets
            .into_iter()
            .map(|target| SyncPreviewTarget {
                skill_id: target.skill_id,
                skill_name: target.skill_name,
                tool: target.tool,
                target_path: target.target.to_string_lossy().to_string(),
                mode: target.mode.as_str().to_string(),
            })
            .collect()
    })
}

/// Decide which `SyncMode` `is_target_current` should compare against, or
/// `None` if the existing target's mode is incompatible with the desired
/// mode and the skip path must be refused.
///
/// Returns `Some(existing)` when both modes match exactly. Also returns
/// `Some(Copy)` when the existing record is `"copy"` but the desired
/// mode is `Symlink` — this is the Windows fallback case (issue #153):
/// `symlink_dir()` failed on a prior run and we landed in copy mode, so
/// every subsequent startup would re-attempt symlink, fail again, and
/// trigger a full recursive copy. Treating the existing copy as
/// compatible lets the hash gate skip when the source hasn't changed.
///
/// The reverse direction (existing `"symlink"`, desired `Copy`) returns
/// `None` because the user actively changed the `sync_mode` setting and
/// the on-disk symlink doesn't reflect that intent.
fn skip_check_mode(existing_mode: &str, desired: sync_engine::SyncMode) -> Option<sync_engine::SyncMode> {
    match (existing_mode, desired) {
        ("symlink", sync_engine::SyncMode::Symlink) => Some(sync_engine::SyncMode::Symlink),
        ("copy", sync_engine::SyncMode::Copy) => Some(sync_engine::SyncMode::Copy),
        ("copy", sync_engine::SyncMode::Symlink) => Some(sync_engine::SyncMode::Copy),
        _ => None,
    }
}

/// Sync one skill directory to one tool target and record the outcome as a
/// `skill_targets` row — the single write path for the "(disk sync, DB row)"
/// pair. Every caller that establishes a sync goes through here so the row
/// shape (fresh UUID, `status: ok`, `synced_at: now`, recorded `source_hash`
/// for the #153 freshness gate) can never drift between entry points.
///
/// Errors are returned, not logged: each caller owns its policy (skip and
/// count, warn and continue, or propagate). `ErrorKind::Io` means the disk
/// sync failed (nothing recorded); `ErrorKind::Database` means the disk sync
/// succeeded but the row insert failed — the next startup pass detects the
/// missing row and re-syncs, so both are safe to surface as "failed".
///
/// The hash-refresh partial update in `resync_copy_targets` intentionally
/// does not use this: it preserves the existing row identity instead of
/// minting a new one.
pub(crate) fn sync_pair(
    store: &SkillStore,
    pair: &ScenarioSyncTarget,
) -> Result<sync_engine::SyncMode, AppError> {
    let actual_mode =
        sync_engine::sync_skill(&pair.source, &pair.target, pair.mode).map_err(AppError::io)?;
    let record = SkillTargetRecord {
        id: uuid::Uuid::new_v4().to_string(),
        skill_id: pair.skill_id.clone(),
        tool: pair.tool.clone(),
        target_path: pair.target.to_string_lossy().to_string(),
        mode: actual_mode.as_str().to_string(),
        status: "ok".to_string(),
        synced_at: Some(chrono::Utc::now().timestamp_millis()),
        last_error: None,
        source_hash: pair.source_hash.clone(),
    };
    store.insert_target(&record).map_err(AppError::db)?;
    Ok(actual_mode)
}

/// Tear down one (skill, tool) sync: remove the files, then drop the
/// `skill_targets` row — the single write path mirroring [`sync_pair`].
///
/// Two invariants live here so no call site can get them wrong:
///
/// - **Removal first, row second.** When the disk removal fails the row is
///   kept and the error returned, so the pair stays visible and retryable
///   instead of silently leaving an orphaned directory behind (the same
///   decision as batch remove).
/// - **Shared paths survive.** Several tools can resolve to the same skills
///   directory, so a physical path may back more than one row. The files are
///   only removed when this pair owns the last row pointing at them;
///   otherwise only the row is dropped.
///
/// Batch remove (`apply_remove`) implements the same rules inline because it
/// evaluates shared-ness across the whole batch at once.
pub(crate) fn unsync_pair(
    store: &SkillStore,
    skill_id: &str,
    tool: &str,
    target_path: &Path,
) -> Result<(), AppError> {
    let shared = store
        .get_all_targets()
        .map_err(AppError::db)?
        .iter()
        .any(|t| {
            !(t.skill_id == skill_id && t.tool == tool)
                && Path::new(&t.target_path) == target_path
        });
    if !shared {
        sync_engine::remove_target(target_path).map_err(AppError::io)?;
    }
    store.delete_target(skill_id, tool).map_err(AppError::db)?;
    Ok(())
}

pub fn sync_desired_targets(
    store: &SkillStore,
    desired_targets: &[ScenarioSyncTarget],
) -> Result<(), AppError> {
    let batch_start = Instant::now();
    let existing_targets: HashMap<(String, String), SkillTargetRecord> = store
        .get_all_targets()
        .map_err(AppError::db)?
        .into_iter()
        .map(|target| ((target.skill_id.clone(), target.tool.clone()), target))
        .collect();

    let mut synced_count = 0usize;
    let mut skipped_count = 0usize;
    let mut failed_count = 0usize;

    for desired in desired_targets {
        let target_start = Instant::now();
        let key = (desired.skill_id.clone(), desired.tool.clone());
        if let Some(existing) = existing_targets.get(&key) {
            let target_path = PathBuf::from(&existing.target_path);
            if target_path != desired.target {
                // Replace flow: even when the stale removal fails, the
                // sync_pair below upserts the row onto the new path, so a
                // warning (not an abort) is the right severity here.
                if let Err(e) = unsync_pair(store, &desired.skill_id, &desired.tool, &target_path)
                {
                    log::warn!(
                        "Failed to clean up stale target {} for skill {}, tool {}: {e}",
                        target_path.display(),
                        desired.skill_id,
                        desired.tool
                    );
                }
            } else if existing.status == "ok" {
                if let Some(check_mode) = skip_check_mode(&existing.mode, desired.mode) {
                    if sync_engine::is_target_current(
                        &desired.source,
                        &desired.target,
                        check_mode,
                        existing.source_hash.as_deref(),
                        desired.source_hash.as_deref(),
                    ) {
                        // Surface the Windows fallback case in logs so operators
                        // can tell when a target is permanently on Copy because
                        // an earlier symlink_dir() failed (issue #153). Helpful
                        // when a user later enables Developer Mode and wonders
                        // why Symlink isn't being re-attempted.
                        if existing.mode == "copy"
                            && matches!(desired.mode, sync_engine::SyncMode::Symlink)
                        {
                            log::debug!(
                                "sync_desired_targets: skill {} ({}) staying on copy fallback for {} (content unchanged); trigger a manual resync to retry symlink",
                                desired.skill_id,
                                desired.skill_name,
                                desired.tool
                            );
                        }
                        skipped_count += 1;
                        continue;
                    }
                }
            }
        }

        match sync_pair(store, desired) {
            Ok(actual_mode) => {
                synced_count += 1;
                let elapsed = target_start.elapsed().as_millis();
                if elapsed >= 200 {
                    log::warn!(
                        "sync_desired_targets: slow sync ({elapsed} ms, mode={}) for skill {} ({}) -> {}",
                        actual_mode.as_str(),
                        desired.skill_id,
                        desired.skill_name,
                        desired.target.display()
                    );
                }
            }
            Err(e) => {
                failed_count += 1;
                log::warn!(
                    "Failed to sync skill {} ({}) to {} after {} ms: {e}",
                    desired.skill_id,
                    desired.skill_name,
                    desired.target.display(),
                    target_start.elapsed().as_millis()
                );
            }
        }
    }

    log::info!(
        "sync_desired_targets: {} targets in {} ms (synced={synced_count}, skipped={skipped_count}, failed={failed_count})",
        desired_targets.len(),
        batch_start.elapsed().as_millis()
    );

    Ok(())
}

pub fn unsync_obsolete_scenario_targets(
    store: &SkillStore,
    old_scenario_id: &str,
    desired_targets: &[ScenarioSyncTarget],
) -> Result<(), AppError> {
    let desired_paths: HashMap<(String, String), PathBuf> = desired_targets
        .iter()
        .map(|target| {
            (
                (target.skill_id.clone(), target.tool.clone()),
                target.target.clone(),
            )
        })
        .collect();

    let old_skill_ids = store
        .get_skill_ids_for_scenario(old_scenario_id)
        .map_err(AppError::db)?;
    for skill_id in &old_skill_ids {
        // Cleanup loop policy: log and move to the next unit instead of
        // aborting, so one broken skill doesn't leave every other target
        // of the old scenario behind.
        let targets = match store.get_targets_for_skill(skill_id) {
            Ok(targets) => targets,
            Err(e) => {
                log::warn!(
                    "unsync_obsolete_scenario_targets: failed to list targets for skill {skill_id}, skipping: {e}"
                );
                continue;
            }
        };
        for target in &targets {
            let path = PathBuf::from(&target.target_path);
            let key = (skill_id.clone(), target.tool.clone());
            if desired_paths.get(&key) == Some(&path) {
                continue;
            }

            if let Err(e) = unsync_pair(store, skill_id, &target.tool, &path) {
                log::warn!(
                    "Failed to unsync obsolete target {} for skill {skill_id}, tool {}: {e}",
                    path.display(),
                    target.tool
                );
            }
        }
    }

    Ok(())
}

pub fn unsync_scenario_skills(store: &SkillStore, scenario_id: &str) -> Result<(), AppError> {
    let skill_ids = store
        .get_skill_ids_for_scenario(scenario_id)
        .map_err(AppError::db)?;

    for skill_id in &skill_ids {
        // Same cleanup-loop policy as unsync_obsolete_scenario_targets:
        // log and continue rather than abort the remaining skills.
        let targets = match store.get_targets_for_skill(skill_id) {
            Ok(targets) => targets,
            Err(e) => {
                log::warn!(
                    "unsync_scenario_skills: failed to list targets for skill {skill_id}, skipping: {e}"
                );
                continue;
            }
        };
        for target in &targets {
            let path = PathBuf::from(&target.target_path);
            if let Err(e) = unsync_pair(store, skill_id, &target.tool, &path) {
                log::warn!(
                    "Failed to unsync target {} for skill {skill_id}, tool {}: {e}",
                    path.display(),
                    target.tool
                );
            }
        }
    }

    Ok(())
}

pub fn sync_scenario_skills(store: &SkillStore, scenario_id: &str) -> Result<(), AppError> {
    let desired_targets = collect_scenario_sync_targets(store, scenario_id)?;
    sync_desired_targets(store, &desired_targets)
}

pub fn apply_scenario_to_default(store: &SkillStore, scenario_id: &str) -> Result<(), AppError> {
    ensure_scenario_exists(store, scenario_id)?;
    let desired_targets = collect_scenario_sync_targets(store, scenario_id)?;

    // A failed read here must abort: proceeding without knowing the old
    // scenario would skip the obsolete-target cleanup and leave the previous
    // scenario's files on disk with live-looking DB rows.
    if let Some(old_id) = store.get_active_scenario_id().map_err(AppError::db)? {
        if old_id != scenario_id {
            unsync_obsolete_scenario_targets(store, &old_id, &desired_targets)?;
        }
    }

    store.set_active_scenario(scenario_id).map_err(AppError::db)?;
    sync_desired_targets(store, &desired_targets)
}

pub fn sync_skill_to_active_scenario(
    store: &SkillStore,
    scenario_id: &str,
    skill_id: &str,
) -> Result<(), AppError> {
    if let Some(active_id) = store.get_active_scenario_id().map_err(AppError::db)? {
        if active_id == scenario_id {
            let adapters = enabled_installed_adapters_for_scenario_skill(store, scenario_id, skill_id)?;
            let configured_mode = store.get_setting("sync_mode").map_err(AppError::db)?;
            let Some(skill) = store.get_skill_by_id(skill_id).map_err(AppError::db)? else {
                log::debug!(
                    "sync_skill_to_active_scenario: skill {skill_id} no longer exists, nothing to sync"
                );
                return Ok(());
            };
            let source = PathBuf::from(&skill.central_path);
            let target_name = sync_engine::target_dir_name(&source, &skill.name);
            let old_targets = store.get_targets_for_skill(skill_id).unwrap_or_else(|e| {
                log::warn!(
                    "sync_skill_to_active_scenario: failed to list existing targets for skill {skill_id}, skipping stale-path cleanup: {e}"
                );
                Vec::new()
            });
            for adapter in &adapters {
                if let Some(old) = old_targets.iter().find(|t| t.tool == adapter.key) {
                    let old_path = PathBuf::from(&old.target_path);
                    if old_path != adapter.skills_dir().join(&target_name) {
                        // Replace flow: the sync_pair below re-upserts the row,
                        // so a failed stale cleanup is a warning, not an abort.
                        if let Err(e) = unsync_pair(store, skill_id, &adapter.key, &old_path) {
                            log::warn!(
                                "Failed to clean up stale target {}: {e}",
                                old_path.display()
                            );
                        }
                    }
                }

                let pair = ScenarioSyncTarget {
                    skill_id: skill_id.to_string(),
                    skill_name: skill.name.clone(),
                    tool: adapter.key.clone(),
                    source: source.clone(),
                    target: adapter.skills_dir().join(&target_name),
                    mode: sync_engine::sync_mode_for_tool(&adapter.key, configured_mode.as_deref()),
                    source_hash: skill.content_hash.clone(),
                };
                if let Err(e) = sync_pair(store, &pair) {
                    log::warn!(
                        "Failed to sync skill {skill_id} to {}: {e}",
                        pair.target.display()
                    );
                }
            }
        }
    }
    Ok(())
}

pub fn ensure_default_startup_scenario(store: &SkillStore) -> Result<(), AppError> {
    let mut scenarios = store.get_all_scenarios().map_err(AppError::db)?;
    if scenarios.is_empty() {
        let now = chrono::Utc::now().timestamp_millis();
        let default_scenario = ScenarioRecord {
            id: uuid::Uuid::new_v4().to_string(),
            name: "Default".to_string(),
            description: Some("Default startup scenario".to_string()),
            icon: None,
            sort_order: 0,
            created_at: now,
            updated_at: now,
        };
        store.insert_scenario(&default_scenario).map_err(AppError::db)?;
        scenarios.push(default_scenario);
    }

    let current_active = store.get_active_scenario_id().map_err(AppError::db)?;
    let preferred_default = store.get_setting("default_scenario").ok().flatten();

    let desired_active = preferred_default
        .filter(|id| scenarios.iter().any(|scenario| scenario.id == *id))
        .or_else(|| {
            current_active
                .clone()
                .filter(|id| scenarios.iter().any(|scenario| scenario.id == *id))
        })
        .unwrap_or_else(|| scenarios[0].id.clone());

    if current_active.as_deref() != Some(desired_active.as_str()) {
        if let Some(old_active) = current_active.as_deref() {
            unsync_scenario_skills(store, old_active)?;
        }
        store
            .set_active_scenario(&desired_active)
            .map_err(AppError::db)?;
    }

    sync_scenario_skills(store, &desired_active)
}

pub fn ensure_cli_scenario_state(store: &SkillStore) -> Result<(), AppError> {
    let mut scenarios = store.get_all_scenarios().map_err(AppError::db)?;
    if scenarios.is_empty() {
        let now = chrono::Utc::now().timestamp_millis();
        let default_scenario = ScenarioRecord {
            id: uuid::Uuid::new_v4().to_string(),
            name: "Default".to_string(),
            description: Some("Default startup scenario".to_string()),
            icon: None,
            sort_order: 0,
            created_at: now,
            updated_at: now,
        };
        store.insert_scenario(&default_scenario).map_err(AppError::db)?;
        scenarios.push(default_scenario);
    }

    let current_active = store.get_active_scenario_id().map_err(AppError::db)?;
    if current_active
        .as_deref()
        .is_some_and(|id| scenarios.iter().any(|scenario| scenario.id == id))
    {
        return Ok(());
    }

    let preferred_default = store.get_setting("default_scenario").ok().flatten();
    let desired_active = preferred_default
        .filter(|id| scenarios.iter().any(|scenario| scenario.id == *id))
        .unwrap_or_else(|| scenarios[0].id.clone());

    store
        .set_active_scenario(&desired_active)
        .map_err(AppError::db)
}

pub fn restore_all_skills_sync_included(store: &SkillStore) -> Result<bool, AppError> {
    let mut changed = false;
    for skill in store.get_all_skills().map_err(AppError::db)? {
        if !skill.enabled {
            store
                .update_skill_enabled(&skill.id, true)
                .map_err(AppError::db)?;
            changed = true;
        }
    }
    Ok(changed)
}

/// Fire-and-forget by design (called from tool-detection callbacks with no
/// error channel), so every failure is logged rather than silently dropped.
pub fn sync_active_scenario_to_tool(store: &SkillStore, tool_key: &str) {
    let active_id = match store.get_active_scenario_id() {
        Ok(Some(id)) => id,
        Ok(None) => return,
        Err(e) => {
            log::warn!("sync_active_scenario_to_tool: failed to read active scenario: {e}");
            return;
        }
    };
    let skill_ids = match store.get_skill_ids_for_scenario(&active_id) {
        Ok(ids) => ids,
        Err(e) => {
            log::warn!(
                "sync_active_scenario_to_tool: failed to list skills for scenario {active_id}: {e}"
            );
            return;
        }
    };
    for skill_id in skill_ids {
        match enabled_installed_adapters_for_scenario_skill(store, &active_id, &skill_id) {
            Ok(adapters) => {
                if adapters.iter().any(|adapter| adapter.key == tool_key) {
                    if let Err(e) = sync_skill_to_active_scenario(store, &active_id, &skill_id) {
                        log::warn!(
                            "sync_active_scenario_to_tool: failed to sync skill {skill_id} to {tool_key}: {e}"
                        );
                    }
                }
            }
            Err(e) => {
                log::warn!(
                    "sync_active_scenario_to_tool: failed to resolve adapters for skill {skill_id}: {e}"
                );
            }
        }
    }
}

pub fn sync_single_skill_to_tool(
    store: &SkillStore,
    skill_id: &str,
    tool: &str,
) -> Result<(), AppError> {
    let adapter = tool_adapters::find_adapter_with_store(store, tool)
        .ok_or_else(|| AppError::not_found(format!("Unknown tool: {}", tool)))?;

    if !adapter.is_installed() {
        return Err(AppError::not_found(format!(
            "{} is not installed",
            adapter.display_name
        )));
    }

    if tool_service::get_disabled_tools(store).contains(&tool.to_string()) {
        return Err(AppError::invalid_input(format!(
            "{} is disabled",
            adapter.display_name
        )));
    }

    let skill = store
        .get_skill_by_id(skill_id)
        .map_err(AppError::db)?
        .ok_or_else(|| AppError::not_found("Skill not found"))?;

    let source = PathBuf::from(&skill.central_path);
    let target = adapter
        .skills_dir()
        .join(sync_engine::target_dir_name(&source, &skill.name));
    let configured_mode = store.get_setting("sync_mode").map_err(AppError::db)?;
    let pair = ScenarioSyncTarget {
        skill_id: skill_id.to_string(),
        skill_name: skill.name.clone(),
        tool: tool.to_string(),
        source,
        target,
        mode: sync_engine::sync_mode_for_tool(tool, configured_mode.as_deref()),
        source_hash: skill.content_hash.clone(),
    };
    sync_pair(store, &pair)?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub enum BatchApplyMode {
    Add,
    Remove,
}

/// Aggregate outcome of a [`apply_skills_to_tools`] batch. `applied` counts
/// every pair processed (including idempotent re-syncs of an already-synced
/// pair, since `insert_target` upserts), `failed` counts pairs whose
/// sync/insert/remove raised an error.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct BatchApplyResult {
    pub applied: usize,
    pub failed: usize,
}

/// Propagate a change in a skill's *actual* sync state (`skill_targets`) to
/// the per-scenario toggle table (`scenario_skill_tools`) for **every**
/// scenario the skill belongs to.
///
/// `skill_targets` is global: a single row means "this skill is currently
/// synced to this tool on disk". `scenario_skill_tools` is per-scenario
/// intent ("when this preset is active, sync the skill to this tool").
/// Whenever a *global* operation (batch apply, or a single sync/unsync from
/// the skill list) adds or removes a target, the toggle must be updated in
/// **every** scenario that contains the skill — not only the active one.
/// Otherwise the detail panel of a non-active preset shows a stale
/// "enabled" for a tool that is no longer (or not yet) actually synced,
/// which is exactly the "UI says enabled but nothing on disk" bug.
pub(crate) fn propagate_target_toggle(
    store: &SkillStore,
    skill_id: &str,
    tool: &str,
    enabled: bool,
) -> Result<(), AppError> {
    let all_adapter_keys: Vec<String> = tool_adapters::enabled_installed_adapters(store)
        .iter()
        .map(|a| a.key.clone())
        .collect();
    let scenario_ids = store.get_scenarios_for_skill(skill_id).map_err(|e| {
        AppError::db(format!(
            "propagate_target_toggle: failed to list scenarios for skill {skill_id}: {e}"
        ))
    })?;
    for scenario_id in scenario_ids {
        store
            .ensure_scenario_skill_tool_defaults(&scenario_id, skill_id, &all_adapter_keys)
            .map_err(|e| {
                AppError::db(format!(
                    "propagate_target_toggle: failed to seed tool defaults for scenario {scenario_id}, skill {skill_id}: {e}"
                ))
            })?;
        store
            .set_scenario_skill_tool_enabled(&scenario_id, skill_id, tool, enabled)
            .map_err(|e| {
                AppError::db(format!(
                    "propagate_target_toggle: failed to set toggle {enabled} for scenario {scenario_id}, skill {skill_id}, tool {tool}: {e}"
                ))
            })?;
    }
    Ok(())
}

/// Apply a batch of `(skill_id × tool_key)` pairs in either Add or Remove
/// mode.
///
/// This is the tray-side preset apply primitive and the MySkills batch-sync
/// primitive. Unlike [`sync_single_skill_to_tool`] (wrapped by the
/// `sync_skill_to_tool` Tauri command) it does not touch `active_scenario_id`,
/// but it does mirror each on-disk change into the `scenario_skill_tools`
/// toggles of every affected scenario via [`propagate_target_toggle`] so the
/// detail panels never go stale.
///
/// Remove mode handles shared physical paths: a `target_path` may be referenced
/// by multiple `(skill_id, tool)` records when several tools resolve to the same
/// skills directory. The filesystem path is only removed when no remaining
/// `skill_targets` row references it after the batch deletions, so removing one
/// preset's tools never wipes another tool's still-active files.
pub fn apply_skills_to_tools(
    store: &SkillStore,
    skill_ids: &[String],
    tool_keys: &[String],
    mode: BatchApplyMode,
) -> Result<BatchApplyResult, AppError> {
    if skill_ids.is_empty() || tool_keys.is_empty() {
        return Ok(BatchApplyResult::default());
    }

    // All three entry points (batch_apply_skills, apply_preset_to_coding_agents,
    // tray preset click) funnel through here, so the central-repo lock and the
    // DB→metadata flush live here rather than at each call site. The lock
    // serializes with set_skill_tool_toggle and background repo writers; the
    // flush keeps the membership JSON's `tools` map in step with the
    // `scenario_skill_tools` rows this batch just updated — otherwise the next
    // startup reindex would restore the pre-batch toggles from stale JSON.
    // Callers must not already hold the repo lock (it is not reentrant).
    let _lock = RepoLock::acquire_foreground("batch apply skills").map_err(AppError::db)?;

    let result = match mode {
        BatchApplyMode::Add => apply_add(store, skill_ids, tool_keys),
        BatchApplyMode::Remove => apply_remove(store, skill_ids, tool_keys),
    }?;

    sync_metadata::write_all_from_db_unlocked(store).map_err(AppError::db)?;
    Ok(result)
}

fn apply_add(
    store: &SkillStore,
    skill_ids: &[String],
    tool_keys: &[String],
) -> Result<BatchApplyResult, AppError> {
    let configured_mode = store.get_setting("sync_mode").map_err(AppError::db)?;
    let disabled = tool_service::get_disabled_tools(store);

    let mut adapters: HashMap<String, tool_adapters::ToolAdapter> = HashMap::new();
    for key in tool_keys {
        if disabled.contains(key) {
            log::debug!("apply_skills_to_tools: skipping disabled tool {key}");
            continue;
        }
        let Some(adapter) = tool_adapters::find_adapter_with_store(store, key) else {
            log::warn!("apply_skills_to_tools: unknown tool {key}");
            continue;
        };
        if !adapter.is_installed() {
            log::debug!(
                "apply_skills_to_tools: skipping uninstalled tool {} ({key})",
                adapter.display_name
            );
            continue;
        }
        adapters.insert(key.clone(), adapter);
    }

    let mut applied = 0usize;
    let mut failed = 0usize;
    // Only pairs whose sync + insert_target both succeeded may have their
    // scenario toggles flipped to enabled below; propagating for a failed
    // pair would recreate the "UI says enabled but nothing on disk" split.
    let mut synced_ok: HashSet<(String, String)> = HashSet::new();
    for skill_id in skill_ids {
        let Ok(Some(skill)) = store.get_skill_by_id(skill_id) else {
            log::warn!("apply_skills_to_tools: skill {skill_id} not found");
            continue;
        };
        let source = PathBuf::from(&skill.central_path);
        let target_name = sync_engine::target_dir_name(&source, &skill.name);
        for (tool_key, adapter) in &adapters {
            let pair = ScenarioSyncTarget {
                skill_id: skill_id.clone(),
                skill_name: skill.name.clone(),
                tool: tool_key.clone(),
                source: source.clone(),
                target: adapter.skills_dir().join(&target_name),
                mode: sync_engine::sync_mode_for_tool(tool_key, configured_mode.as_deref()),
                source_hash: skill.content_hash.clone(),
            };
            match sync_pair(store, &pair) {
                Ok(_) => {
                    applied += 1;
                    synced_ok.insert((skill_id.clone(), tool_key.clone()));
                }
                Err(e) => {
                    failed += 1;
                    log::warn!(
                        "apply_skills_to_tools: failed to sync skill {skill_id} ({}) to {}: {e}",
                        skill.name,
                        pair.target.display()
                    );
                }
            }
        }
    }

    log::info!(
        "apply_skills_to_tools(Add): skills={} tools={} applied={applied} failed={failed}",
        skill_ids.len(),
        adapters.len(),
    );

    // Batch syncs are global: propagate the new on-disk state to this skill's
    // toggle in *every* scenario it belongs to, not just the active one.
    // Only successfully synced pairs — a failed pair keeps its old toggle.
    for (skill_id, tool_key) in &synced_ok {
        if let Err(e) = propagate_target_toggle(store, skill_id, tool_key, true) {
            log::error!("apply_skills_to_tools(Add): {e}");
        }
    }

    Ok(BatchApplyResult { applied, failed })
}

fn apply_remove(
    store: &SkillStore,
    skill_ids: &[String],
    tool_keys: &[String],
) -> Result<BatchApplyResult, AppError> {
    let tool_set: HashSet<&String> = tool_keys.iter().collect();

    let mut to_delete: Vec<(String, String, PathBuf)> = Vec::new();
    for skill_id in skill_ids {
        // Abort, don't default to empty: this runs before any mutation, and a
        // silently skipped skill would report success while removing nothing.
        let targets = store.get_targets_for_skill(skill_id).map_err(AppError::db)?;
        for target in targets {
            if tool_set.contains(&target.tool) {
                to_delete.push((
                    skill_id.clone(),
                    target.tool.clone(),
                    PathBuf::from(&target.target_path),
                ));
            }
        }
    }

    if to_delete.is_empty() {
        return Ok(BatchApplyResult::default());
    }

    // Shared-path semantics: a target_path may be referenced by several
    // (skill_id, tool) rows when multiple tools resolve to the same skills
    // directory. Only physically delete a path when every row referencing it
    // belongs to this batch; otherwise keep the files and just drop the rows.
    let batch_pairs: HashSet<(String, String)> = to_delete
        .iter()
        .map(|(skill_id, tool, _)| (skill_id.clone(), tool.clone()))
        .collect();
    // Abort on a failed read: defaulting to an empty set would classify every
    // path as unshared and physically delete directories other tools still use.
    let shared_paths: HashSet<PathBuf> = store
        .get_all_targets()
        .map_err(AppError::db)?
        .into_iter()
        .filter(|t| !batch_pairs.contains(&(t.skill_id.clone(), t.tool.clone())))
        .map(|t| PathBuf::from(&t.target_path))
        .collect();

    // Phase 1: physical deletion first. A path that fails to delete keeps
    // every DB row pointing at it — the pair stays visible in the UI and the
    // user can retry — and its pairs are reported in `failed`.
    let candidate_paths: HashSet<PathBuf> = to_delete.iter().map(|(_, _, p)| p.clone()).collect();
    let mut failed_paths: HashSet<PathBuf> = HashSet::new();
    let mut removed = 0usize;
    for path in &candidate_paths {
        if shared_paths.contains(path) {
            log::debug!(
                "apply_skills_to_tools(Remove): keeping {} (still referenced by another target)",
                path.display()
            );
            continue;
        }
        if let Err(e) = sync_engine::remove_target(path) {
            log::warn!(
                "apply_skills_to_tools(Remove): failed to remove {}: {e}",
                path.display()
            );
            failed_paths.insert(path.clone());
        } else {
            removed += 1;
        }
    }

    // Phase 2: drop DB rows and flip toggles only for pairs whose path is
    // actually gone (or intentionally kept because another target shares it).
    let mut applied = 0usize;
    let mut failed = 0usize;
    for (skill_id, tool, path) in &to_delete {
        if failed_paths.contains(path) {
            failed += 1;
            continue;
        }
        if let Err(e) = store.delete_target(skill_id, tool) {
            log::warn!(
                "apply_skills_to_tools(Remove): failed to delete target record for skill {skill_id} / {tool}: {e}"
            );
            failed += 1;
            continue;
        }
        applied += 1;
        // Batch removals are global: flip the toggle to false in *every*
        // scenario that lists the skill, not just the active one.
        if let Err(e) = propagate_target_toggle(store, skill_id, tool, false) {
            log::error!("apply_skills_to_tools(Remove): {e}");
        }
    }

    log::info!(
        "apply_skills_to_tools(Remove): pairs={} fs_removed={removed} failed={failed}",
        to_delete.len(),
    );

    Ok(BatchApplyResult { applied, failed })
}

#[cfg(test)]
mod sync_desired_targets_tests {
    use super::*;
    use crate::core::central_repo;
    use crate::core::skill_store::{SkillRecord, SkillStore, SkillTargetRecord};
    use std::fs;
    use tempfile::tempdir;

    /// Issue #153 regression: when the existing target was written in
    /// Copy mode (Windows symlink fallback) but the configured mode is
    /// Symlink, and the source content hash hasn't changed, the sync
    /// must be skipped. Prior to the fix the mode-equality guard would
    /// reject the skip branch and re-attempt the full recursive copy
    /// every startup.
    #[test]
    fn copy_fallback_target_with_matching_hash_is_skipped() {
        let _lock = central_repo::test_base_dir_lock();
        let tmp = tempdir().unwrap();
        let base = tmp.path().join("repo");
        central_repo::set_test_base_dir_override(Some(base.clone()));
        fs::create_dir_all(central_repo::skills_dir()).unwrap();
        let store = SkillStore::new(&base.join("test.db")).unwrap();

        // Real source dir with one file (the central skill).
        let source = central_repo::skills_dir().join("skill-a");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("SKILL.md"), "real source").unwrap();

        // Pre-existing target dir with a marker file that would be wiped
        // by copy_dir_recursive's pre-clean step if a re-sync ran.
        let target = tmp.path().join("agent-skills").join("skill-a");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("MARKER.txt"), "do not wipe me").unwrap();

        // DB rows: skill content_hash = "h1"; existing target also at "h1",
        // mode "copy" (i.e. previously fell back from Symlink).
        let skill = SkillRecord {
            id: "skill-a".to_string(),
            name: "skill-a".to_string(),
            description: None,
            source_type: "import".to_string(),
            source_ref: Some(source.to_string_lossy().to_string()),
            source_ref_resolved: None,
            source_subpath: None,
            source_branch: None,
            source_revision: None,
            remote_revision: None,
            central_path: source.to_string_lossy().to_string(),
            content_hash: Some("h1".to_string()),
            enabled: true,
            created_at: 1,
            updated_at: 1,
            status: "ok".to_string(),
            update_status: "local_only".to_string(),
            last_checked_at: None,
            last_check_error: None,
        };
        store.insert_skill(&skill).unwrap();

        store
            .insert_target(&SkillTargetRecord {
                id: "target-1".to_string(),
                skill_id: "skill-a".to_string(),
                tool: "claude-code".to_string(),
                target_path: target.to_string_lossy().to_string(),
                mode: "copy".to_string(),
                status: "ok".to_string(),
                synced_at: Some(1),
                last_error: None,
                source_hash: Some("h1".to_string()),
            })
            .unwrap();

        // Desired target: same source/target/hash but Symlink mode
        // (the configured default that originally fell back to Copy).
        let desired = vec![ScenarioSyncTarget {
            skill_id: "skill-a".to_string(),
            skill_name: "skill-a".to_string(),
            tool: "claude-code".to_string(),
            source: source.clone(),
            target: target.clone(),
            mode: sync_engine::SyncMode::Symlink,
            source_hash: Some("h1".to_string()),
        }];

        sync_desired_targets(&store, &desired).unwrap();

        // The marker file proves no re-sync ran (a real re-sync would
        // have called copy_dir_recursive after wiping the target).
        assert!(
            target.join("MARKER.txt").exists(),
            "target dir was wiped — skip did not fire"
        );
        // The skill's actual SKILL.md should NOT have been copied in,
        // because we skipped the sync entirely.
        assert!(
            !target.join("SKILL.md").exists(),
            "SKILL.md appeared — sync ran instead of skipping"
        );

        central_repo::set_test_base_dir_override(None);
    }

    /// Companion: if the target has been manually deleted, even with a
    /// matching hash, we must NOT skip — the user's agent dir is
    /// otherwise left broken.
    #[test]
    fn deleted_target_with_matching_hash_forces_resync() {
        let _lock = central_repo::test_base_dir_lock();
        let tmp = tempdir().unwrap();
        let base = tmp.path().join("repo");
        central_repo::set_test_base_dir_override(Some(base.clone()));
        fs::create_dir_all(central_repo::skills_dir()).unwrap();
        let store = SkillStore::new(&base.join("test.db")).unwrap();

        let source = central_repo::skills_dir().join("skill-b");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("SKILL.md"), "real source").unwrap();

        // Target path that does NOT exist on disk.
        let target = tmp.path().join("agent-skills").join("skill-b");

        let skill = SkillRecord {
            id: "skill-b".to_string(),
            name: "skill-b".to_string(),
            description: None,
            source_type: "import".to_string(),
            source_ref: Some(source.to_string_lossy().to_string()),
            source_ref_resolved: None,
            source_subpath: None,
            source_branch: None,
            source_revision: None,
            remote_revision: None,
            central_path: source.to_string_lossy().to_string(),
            content_hash: Some("h1".to_string()),
            enabled: true,
            created_at: 1,
            updated_at: 1,
            status: "ok".to_string(),
            update_status: "local_only".to_string(),
            last_checked_at: None,
            last_check_error: None,
        };
        store.insert_skill(&skill).unwrap();

        store
            .insert_target(&SkillTargetRecord {
                id: "target-2".to_string(),
                skill_id: "skill-b".to_string(),
                tool: "claude-code".to_string(),
                target_path: target.to_string_lossy().to_string(),
                mode: "copy".to_string(),
                status: "ok".to_string(),
                synced_at: Some(1),
                last_error: None,
                source_hash: Some("h1".to_string()),
            })
            .unwrap();

        let desired = vec![ScenarioSyncTarget {
            skill_id: "skill-b".to_string(),
            skill_name: "skill-b".to_string(),
            tool: "claude-code".to_string(),
            source: source.clone(),
            target: target.clone(),
            mode: sync_engine::SyncMode::Copy,
            source_hash: Some("h1".to_string()),
        }];

        sync_desired_targets(&store, &desired).unwrap();

        // Sync must have run — target should now exist with the source content.
        assert!(target.join("SKILL.md").exists(), "missing target was not re-synced");

        central_repo::set_test_base_dir_override(None);
    }

    /// Regression for the "UI shows enabled but not synced" bug.
    ///
    /// `apply_skills_to_tools` operates on the global `skill_targets` table,
    /// so removing a `(skill, tool)` pair must flip the `scenario_skill_tools`
    /// toggle to false in **every** scenario that lists the skill — not only
    /// the active one. Before the fix only the active scenario was updated,
    /// which left non-active presets' detail panels showing the tool as
    /// enabled even though nothing was on disk anymore.
    #[test]
    fn batch_remove_flips_toggle_in_all_scenarios() {
        use crate::core::skill_store::ScenarioRecord;
        use crate::core::tool_adapters::CustomToolDef;

        let _lock = central_repo::test_base_dir_lock();
        let tmp = tempdir().unwrap();
        let base = tmp.path().join("repo");
        central_repo::set_test_base_dir_override(Some(base.clone()));
        fs::create_dir_all(central_repo::skills_dir()).unwrap();
        let store = SkillStore::new(&base.join("test.db")).unwrap();

        // A single custom tool is the only eligible target.
        let agent_dir = tmp.path().join("agent-skills");
        fs::create_dir_all(&agent_dir).unwrap();
        let custom_tools = vec![CustomToolDef {
            key: "test_agent".to_string(),
            display_name: "Test Agent".to_string(),
            skills_dir: agent_dir.to_string_lossy().to_string(),
            project_relative_skills_dir: None,
            category: Default::default(),
        }];
        store
            .set_setting("custom_tools", &serde_json::to_string(&custom_tools).unwrap())
            .unwrap();
        let disabled_builtin: Vec<String> = tool_adapters::default_tool_adapters()
            .into_iter()
            .map(|a| a.key)
            .collect();
        store
            .set_setting("disabled_tools", &serde_json::to_string(&disabled_builtin).unwrap())
            .unwrap();

        // Central skill source.
        let source = central_repo::skills_dir().join("skill-x");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("SKILL.md"), "---\nname: skill-x\n---\n").unwrap();
        store
            .insert_skill(&SkillRecord {
                id: "skill-x".to_string(),
                name: "skill-x".to_string(),
                description: None,
                source_type: "import".to_string(),
                source_ref: Some(source.to_string_lossy().to_string()),
                source_ref_resolved: None,
                source_subpath: None,
                source_branch: None,
                source_revision: None,
                remote_revision: None,
                central_path: source.to_string_lossy().to_string(),
                content_hash: None,
                enabled: true,
                created_at: 1,
                updated_at: 1,
                status: "ok".to_string(),
                update_status: "local_only".to_string(),
                last_checked_at: None,
                last_check_error: None,
            })
            .unwrap();

        // Two scenarios, both contain the skill; A is active.
        for id in ["A", "B"] {
            store
                .insert_scenario(&ScenarioRecord {
                    id: id.to_string(),
                    name: id.to_string(),
                    description: None,
                    icon: None,
                    sort_order: 0,
                    created_at: 1,
                    updated_at: 1,
                })
                .unwrap();
            store.add_skill_to_scenario(id, "skill-x").unwrap();
        }
        store.set_active_scenario("A").unwrap();

        let enabled_tools = |sid: &str| {
            store
                .get_enabled_tools_for_scenario_skill(sid, "skill-x")
                .unwrap()
        };

        // Add: syncs on disk and must enable the toggle in both scenarios.
        apply_skills_to_tools(
            &store,
            &["skill-x".to_string()],
            &["test_agent".to_string()],
            BatchApplyMode::Add,
        )
        .unwrap();
        assert!(enabled_tools("A").contains(&"test_agent".to_string()));
        assert!(
            enabled_tools("B").contains(&"test_agent".to_string()),
            "Add should propagate toggle to non-active scenario too"
        );

        // Remove: the bug path. Must clear the toggle in BOTH scenarios.
        apply_skills_to_tools(
            &store,
            &["skill-x".to_string()],
            &["test_agent".to_string()],
            BatchApplyMode::Remove,
        )
        .unwrap();
        let targets = store.get_targets_for_skill("skill-x").unwrap();
        assert!(!targets.iter().any(|t| t.tool == "test_agent"));
        assert!(
            !enabled_tools("A").contains(&"test_agent".to_string()),
            "active scenario toggle should be cleared"
        );
        assert!(
            !enabled_tools("B").contains(&"test_agent".to_string()),
            "non-active scenario toggle must also be cleared (regression)"
        );

        central_repo::set_test_base_dir_override(None);
    }
}

#[cfg(test)]
mod apply_skills_to_tools_tests {
    use super::*;
    use crate::core::central_repo;
    use crate::core::skill_store::{ScenarioRecord, SkillRecord, SkillStore};
    use crate::core::tool_adapters::CustomToolDef;
    use std::fs;
    use tempfile::tempdir;

    fn insert_test_skill(store: &SkillStore, id: &str) -> std::path::PathBuf {
        let source = central_repo::skills_dir().join(id);
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("SKILL.md"), format!("---\nname: {id}\n---\n")).unwrap();
        store
            .insert_skill(&SkillRecord {
                id: id.to_string(),
                name: id.to_string(),
                description: None,
                source_type: "import".to_string(),
                source_ref: Some(source.to_string_lossy().to_string()),
                source_ref_resolved: None,
                source_subpath: None,
                source_branch: None,
                source_revision: None,
                remote_revision: None,
                central_path: source.to_string_lossy().to_string(),
                content_hash: None,
                enabled: true,
                created_at: 1,
                updated_at: 1,
                status: "ok".to_string(),
                update_status: "local_only".to_string(),
                last_checked_at: None,
                last_check_error: None,
            })
            .unwrap();
        source
    }

    fn configure_custom_tools(store: &SkillStore, tools: &[(&str, &std::path::Path)]) {
        let custom_tools: Vec<CustomToolDef> = tools
            .iter()
            .map(|(key, dir)| CustomToolDef {
                key: key.to_string(),
                display_name: key.to_string(),
                skills_dir: dir.to_string_lossy().to_string(),
                project_relative_skills_dir: None,
                category: Default::default(),
            })
            .collect();
        store
            .set_setting("custom_tools", &serde_json::to_string(&custom_tools).unwrap())
            .unwrap();
        let disabled_builtin: Vec<String> = tool_adapters::default_tool_adapters()
            .into_iter()
            .map(|a| a.key)
            .collect();
        store
            .set_setting("disabled_tools", &serde_json::to_string(&disabled_builtin).unwrap())
            .unwrap();
    }

    fn insert_scenario_with_skill(store: &SkillStore, scenario_id: &str, skill_id: &str) {
        store
            .insert_scenario(&ScenarioRecord {
                id: scenario_id.to_string(),
                name: scenario_id.to_string(),
                description: None,
                icon: None,
                sort_order: 0,
                created_at: 1,
                updated_at: 1,
            })
            .unwrap();
        store.add_skill_to_scenario(scenario_id, skill_id).unwrap();
    }

    /// Issue #10 regression: a pair whose sync fails must NOT have its
    /// per-scenario toggle flipped to enabled. Before the fix the
    /// end-of-batch propagation ran over `skill_ids × adapters`, so a user
    /// who had explicitly disabled a tool saw it flip back to enabled even
    /// though the sync to that tool just failed.
    #[test]
    fn apply_add_does_not_enable_toggle_for_failed_pair() {
        let _lock = central_repo::test_base_dir_lock();
        let tmp = tempdir().unwrap();
        let base = tmp.path().join("repo");
        central_repo::set_test_base_dir_override(Some(base.clone()));
        fs::create_dir_all(central_repo::skills_dir()).unwrap();
        let store = SkillStore::new(&base.join("test.db")).unwrap();

        let ok_dir = tmp.path().join("agent-ok");
        fs::create_dir_all(&ok_dir).unwrap();
        // A regular file where the skills dir should be: creating the target
        // directory beneath it fails, so every sync to this tool fails.
        let broken_dir = tmp.path().join("agent-broken");
        fs::write(&broken_dir, "not a directory").unwrap();
        configure_custom_tools(
            &store,
            &[("ok_agent", ok_dir.as_path()), ("broken_agent", broken_dir.as_path())],
        );

        insert_test_skill(&store, "skill-y");
        insert_scenario_with_skill(&store, "A", "skill-y");
        store.set_active_scenario("A").unwrap();

        // The user has explicitly disabled both tools for this skill in the
        // scenario; only the successful sync may re-enable its toggle.
        for tool in ["ok_agent", "broken_agent"] {
            store
                .set_scenario_skill_tool_enabled("A", "skill-y", tool, false)
                .unwrap();
        }

        let result = apply_skills_to_tools(
            &store,
            &["skill-y".to_string()],
            &["ok_agent".to_string(), "broken_agent".to_string()],
            BatchApplyMode::Add,
        )
        .unwrap();
        assert_eq!(result.applied, 1);
        assert_eq!(result.failed, 1);

        let enabled = store
            .get_enabled_tools_for_scenario_skill("A", "skill-y")
            .unwrap();
        assert!(
            enabled.contains(&"ok_agent".to_string()),
            "successful pair must propagate enabled=true"
        );
        assert!(
            !enabled.contains(&"broken_agent".to_string()),
            "failed pair must not be marked enabled (regression #10)"
        );

        central_repo::set_test_base_dir_override(None);
    }

    /// Issue #12/#38 regression: when the physical removal fails, the DB row
    /// must survive (so the pair stays visible and retryable) and the failure
    /// must be reported instead of the hardcoded `failed: 0`.
    #[cfg(unix)]
    #[test]
    fn apply_remove_keeps_db_row_when_fs_removal_fails() {
        use std::os::unix::fs::PermissionsExt;

        let _lock = central_repo::test_base_dir_lock();
        let tmp = tempdir().unwrap();
        let base = tmp.path().join("repo");
        central_repo::set_test_base_dir_override(Some(base.clone()));
        fs::create_dir_all(central_repo::skills_dir()).unwrap();
        let store = SkillStore::new(&base.join("test.db")).unwrap();

        let agent_dir = tmp.path().join("agent");
        fs::create_dir_all(&agent_dir).unwrap();
        configure_custom_tools(&store, &[("test_agent", agent_dir.as_path())]);

        insert_test_skill(&store, "skill-z");
        insert_scenario_with_skill(&store, "A", "skill-z");
        store.set_active_scenario("A").unwrap();

        apply_skills_to_tools(
            &store,
            &["skill-z".to_string()],
            &["test_agent".to_string()],
            BatchApplyMode::Add,
        )
        .unwrap();
        assert_eq!(store.get_targets_for_skill("skill-z").unwrap().len(), 1);

        // Read-only parent: unlinking the synced target from it fails.
        fs::set_permissions(&agent_dir, fs::Permissions::from_mode(0o555)).unwrap();

        let result = apply_skills_to_tools(
            &store,
            &["skill-z".to_string()],
            &["test_agent".to_string()],
            BatchApplyMode::Remove,
        )
        .unwrap();

        // Restore before asserting so the tempdir can always be cleaned up.
        fs::set_permissions(&agent_dir, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(result.failed, 1, "fs removal failure must be counted");
        assert_eq!(result.applied, 0);
        let targets = store.get_targets_for_skill("skill-z").unwrap();
        assert_eq!(
            targets.len(),
            1,
            "DB row must survive a failed physical removal so the user can retry"
        );
        let enabled = store
            .get_enabled_tools_for_scenario_skill("A", "skill-z")
            .unwrap();
        assert!(
            enabled.contains(&"test_agent".to_string()),
            "toggle must stay enabled while the target is still on disk"
        );

        central_repo::set_test_base_dir_override(None);
    }
}

#[cfg(test)]
mod skip_check_mode_tests {
    use super::skip_check_mode;
    use super::sync_engine::SyncMode;

    #[test]
    fn matching_modes_are_compatible() {
        assert!(matches!(
            skip_check_mode("symlink", SyncMode::Symlink),
            Some(SyncMode::Symlink)
        ));
        assert!(matches!(
            skip_check_mode("copy", SyncMode::Copy),
            Some(SyncMode::Copy)
        ));
    }

    #[test]
    fn copy_existing_with_symlink_desired_treated_as_copy() {
        // Windows fallback case (issue #153): record says copy because
        // symlink_dir failed previously. We accept that and let the hash
        // gate decide freshness, instead of re-attempting symlink and
        // triggering a full recopy on every startup.
        assert!(matches!(
            skip_check_mode("copy", SyncMode::Symlink),
            Some(SyncMode::Copy)
        ));
    }

    #[test]
    fn symlink_existing_with_copy_desired_is_incompatible() {
        // User flipped sync_mode setting from symlink to copy — the
        // on-disk symlink no longer reflects intent, must resync.
        assert!(skip_check_mode("symlink", SyncMode::Copy).is_none());
    }

    #[test]
    fn unknown_existing_mode_is_incompatible() {
        assert!(skip_check_mode("garbage", SyncMode::Symlink).is_none());
        assert!(skip_check_mode("", SyncMode::Copy).is_none());
    }

}
