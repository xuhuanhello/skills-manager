use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};

use super::{central_repo, scenario_service, skill_store::SkillStore, sync_metadata, tool_service};

/// Per-stage timings collected during `initialize_store`. The struct is
/// returned to the caller so the log lines can be emitted once
/// `tauri_plugin_log` is registered — anything logged from inside this
/// function would otherwise be dropped because the logger isn't installed
/// until later in `tauri::Builder::setup`. See issue #153.
#[derive(Debug, Clone)]
pub struct StartupTimings {
    pub ensure_central_repo_ms: u128,
    pub open_store_ms: u128,
    pub migrate_legacy_tool_keys_ms: u128,
    pub skill_count: usize,
    /// Time spent flushing the freshly migrated DB into the metadata JSON
    /// (one-shot after the v7 upgrade), `None` when the flush didn't run.
    pub post_v7_flush_ms: Option<u128>,
    /// True when the post-v7 flush ran and failed (non-fatal: the reindex
    /// then restores pre-v7 toggles and the reconciliation is lost until
    /// the next toggle write, but startup continues).
    pub post_v7_flush_failed: bool,
    pub reindex_from_metadata_ms: Option<u128>,
    /// True when `reindex_from_metadata` failed and was skipped (existing DB
    /// state kept). Surfaced via [`StartupTimings::log`] once the logger is up,
    /// since anything logged during `initialize_store` is otherwise dropped.
    pub reindex_failed: bool,
    pub restore_sync_included_ms: u128,
    pub restore_sync_included_changed: bool,
    pub write_all_from_db_ms: Option<u128>,
    pub apply_scenario_ms: u128,
    /// "default_startup" (Tauri app) or "cli" (CLI bin). Defaults to
    /// `"unknown"` so a struct that escapes `initialize_store_inner`
    /// without being fully populated still produces an obvious value in
    /// the log instead of an empty string.
    pub apply_scenario_kind: &'static str,
    pub total_ms: u128,
}

impl Default for StartupTimings {
    fn default() -> Self {
        Self {
            ensure_central_repo_ms: 0,
            open_store_ms: 0,
            migrate_legacy_tool_keys_ms: 0,
            skill_count: 0,
            post_v7_flush_ms: None,
            post_v7_flush_failed: false,
            reindex_from_metadata_ms: None,
            reindex_failed: false,
            restore_sync_included_ms: 0,
            restore_sync_included_changed: false,
            write_all_from_db_ms: None,
            apply_scenario_ms: 0,
            apply_scenario_kind: "unknown",
            total_ms: 0,
        }
    }
}

pub fn initialize_store() -> Result<(Arc<SkillStore>, StartupTimings)> {
    initialize_store_inner(true)
}

pub fn initialize_cli_store() -> Result<Arc<SkillStore>> {
    initialize_store_inner(false).map(|(store, _)| store)
}

fn initialize_store_inner(
    apply_startup_default: bool,
) -> Result<(Arc<SkillStore>, StartupTimings)> {
    let total_start = Instant::now();
    let mut timings = StartupTimings::default();

    let step = Instant::now();
    central_repo::ensure_central_repo().context("Failed to create central repo")?;
    timings.ensure_central_repo_ms = step.elapsed().as_millis();

    let db_path = central_repo::db_path();
    let step = Instant::now();
    let store = Arc::new(SkillStore::new(&db_path).context("Failed to initialize database")?);
    timings.open_store_ms = step.elapsed().as_millis();

    let step = Instant::now();
    tool_service::migrate_legacy_tool_keys(&store)
        .map_err(|e| anyhow::anyhow!(e.to_string()))
        .context("Failed to migrate legacy tool keys")?;
    timings.migrate_legacy_tool_keys_ms = step.elapsed().as_millis();

    timings.skill_count = store.get_all_skills().map(|s| s.len()).unwrap_or(0);

    if sync_metadata::metadata_exists() {
        // One-shot after the v6→v7 upgrade of an existing database: the
        // migration just cleared stale toggles in `scenario_skill_tools`, but
        // the membership JSON still carries the pre-migration values, and the
        // reindex below would write them straight back into the DB. Flush
        // DB→JSON first so the reindex reads back the reconciled state
        // (idempotent). Failure is non-fatal — the reconciliation is undone
        // by the reindex, which is the pre-migration status quo, not a crash.
        if store.upgraded_existing_db_to_v7() {
            let step = Instant::now();
            if let Err(e) = sync_metadata::write_all_from_db(&store) {
                timings.post_v7_flush_failed = true;
                log::warn!(
                    "Post-v7 metadata flush failed; startup reindex may restore pre-v7 toggles: {e:#}"
                );
            }
            timings.post_v7_flush_ms = Some(step.elapsed().as_millis());
        }

        let step = Instant::now();
        // Reindexing reconciles the on-disk metadata snapshot with the DB. It
        // can legitimately fail when the snapshot is inconsistent with the
        // skills directory — a state this app's own multi-machine git sync can
        // produce (e.g. an interrupted pull). Treat that as non-fatal: log it
        // and keep the existing DB state rather than aborting startup, which
        // previously turned a recoverable inconsistency into an unopenable app
        // (the process panicked before any window or error dialog existed).
        match sync_metadata::reindex_from_metadata(&store) {
            Ok(()) => {
                timings.reindex_from_metadata_ms = Some(step.elapsed().as_millis());
            }
            Err(e) => {
                timings.reindex_from_metadata_ms = Some(step.elapsed().as_millis());
                timings.reindex_failed = true;
                log::warn!(
                    "Startup reindex_from_metadata failed, keeping existing DB state: {e:#}"
                );
            }
        }
    }

    let step = Instant::now();
    let changed = scenario_service::restore_all_skills_sync_included(&store)
        .map_err(|e| anyhow::anyhow!(e.to_string()))
        .context("Failed to restore skill sync inclusion")?;
    timings.restore_sync_included_ms = step.elapsed().as_millis();
    timings.restore_sync_included_changed = changed;
    if changed {
        let step = Instant::now();
        sync_metadata::write_all_from_db(&store)
            .context("Failed to persist restored skill sync inclusion")?;
        timings.write_all_from_db_ms = Some(step.elapsed().as_millis());
    }

    let step = Instant::now();
    if apply_startup_default {
        scenario_service::ensure_default_startup_scenario(&store)
            .map_err(|e| anyhow::anyhow!(e.to_string()))
            .context("Failed to initialize startup scenario")?;
        timings.apply_scenario_kind = "default_startup";
    } else {
        scenario_service::ensure_cli_scenario_state(&store)
            .map_err(|e| anyhow::anyhow!(e.to_string()))
            .context("Failed to initialize CLI scenario state")?;
        timings.apply_scenario_kind = "cli";
    }
    timings.apply_scenario_ms = step.elapsed().as_millis();

    timings.total_ms = total_start.elapsed().as_millis();
    Ok((store, timings))
}

impl StartupTimings {
    /// Emit a single human-readable log block from the captured timings.
    /// Called from `tauri::Builder::setup` once `tauri_plugin_log` is
    /// installed; calling it before that point would lose the output to
    /// the no-op default logger.
    pub fn log(&self) {
        log::info!(
            "startup: initialize_store total {} ms (skills={})",
            self.total_ms,
            self.skill_count
        );
        log::info!(
            "startup: ensure_central_repo {} ms, open_store {} ms, migrate_legacy_tool_keys {} ms",
            self.ensure_central_repo_ms,
            self.open_store_ms,
            self.migrate_legacy_tool_keys_ms
        );
        if let Some(ms) = self.post_v7_flush_ms {
            if self.post_v7_flush_failed {
                log::warn!(
                    "startup: post-v7 metadata flush FAILED after {} ms (see earlier warning for cause)",
                    ms
                );
            } else {
                log::info!("startup: post-v7 metadata flush {} ms", ms);
            }
        }
        if let Some(ms) = self.reindex_from_metadata_ms {
            if self.reindex_failed {
                log::warn!(
                    "startup: reindex_from_metadata FAILED after {} ms — kept existing DB state (see earlier warning for cause)",
                    ms
                );
            } else {
                log::info!(
                    "startup: reindex_from_metadata {} ms (skills={})",
                    ms,
                    self.skill_count
                );
            }
        }
        if self.restore_sync_included_changed {
            log::info!(
                "startup: restore_sync_included changed in {} ms, write_all_from_db {} ms",
                self.restore_sync_included_ms,
                self.write_all_from_db_ms.unwrap_or(0)
            );
        } else {
            log::info!(
                "startup: restore_sync_included no-op in {} ms",
                self.restore_sync_included_ms
            );
        }
        log::info!(
            "startup: apply_scenario ({}) {} ms (skills={})",
            self.apply_scenario_kind,
            self.apply_scenario_ms,
            self.skill_count
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::central_repo;
    use crate::core::skill_store::{ScenarioRecord, SkillRecord};
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;

    /// Build a populated store + metadata snapshot inside the overridden
    /// central repo: one skill on disk, scenarios A (active) + B both
    /// containing it, and an enabled `tool_x` toggle in each — with **no**
    /// `skill_targets` row, i.e. the stale state the v7 migration reconciles.
    fn seed_repo_with_stale_toggles(base: &Path) -> String {
        fs::create_dir_all(central_repo::skills_dir()).unwrap();
        let store = crate::core::skill_store::SkillStore::new(&base.join("skills-manager.db"))
            .unwrap();

        let source = central_repo::skills_dir().join("skill-1");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("SKILL.md"), "---\nname: skill-1\n---\n").unwrap();
        store
            .insert_skill(&SkillRecord {
                id: "skill-1".to_string(),
                name: "skill-1".to_string(),
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
            store.add_skill_to_scenario(id, "skill-1").unwrap();
            store
                .set_scenario_skill_tool_enabled(id, "skill-1", "tool_x", true)
                .unwrap();
        }
        store.set_active_scenario("A").unwrap();

        // Persist the pre-migration state into the metadata JSON, exactly as
        // a pre-v7 build would have left it on disk.
        sync_metadata::write_all_from_db(&store).unwrap();
        "skill-1".to_string()
    }

    fn membership_tool_x(scenario: &str) -> bool {
        let path = sync_metadata::metadata_dir()
            .join("scenario-skills")
            .join(scenario)
            .join("skill-1.json");
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        value["tools"]["tool_x"].as_bool().unwrap()
    }

    /// End-to-end replay of the v6→v7 upgrade on a populated repo: the
    /// migration clears the active scenario's stale toggle, the one-shot
    /// flush pushes that into the metadata JSON *before* reindex, and the
    /// reindex reads back the reconciled state instead of restoring the
    /// stale one. Non-active intent (scenario B) survives all three stages.
    #[test]
    fn startup_migration_flush_reindex_end_to_end() {
        let _lock = central_repo::test_base_dir_lock();
        let tmp = tempdir().unwrap();
        let base = tmp.path().join("repo");
        central_repo::set_test_base_dir_override(Some(base.clone()));

        seed_repo_with_stale_toggles(&base);
        // Both memberships carry the stale enabled=true snapshot.
        assert!(membership_tool_x("A") && membership_tool_x("B"));

        // Rewind the schema version so the next open replays v6→v7 as an
        // existing-database upgrade (the store from seeding is closed here).
        {
            let conn = rusqlite::Connection::open(base.join("skills-manager.db")).unwrap();
            conn.pragma_update(None, "user_version", 6).unwrap();
        }

        let store = initialize_cli_store().expect("startup must succeed");

        let enabled = |scenario: &str| {
            store
                .get_enabled_tools_for_scenario_skill(scenario, "skill-1")
                .unwrap()
                .contains(&"tool_x".to_string())
        };
        assert!(
            !enabled("A"),
            "active scenario's stale toggle must be cleared and stay cleared through reindex"
        );
        assert!(
            enabled("B"),
            "non-active scenario intent must survive migration + flush + reindex"
        );
        assert!(
            !membership_tool_x("A"),
            "flush must land the reconciled toggle in the metadata JSON before reindex"
        );
        assert!(membership_tool_x("B"));
        assert_eq!(store.get_all_skills().unwrap().len(), 1, "reindex must keep the skill");

        central_repo::set_test_base_dir_override(None);
    }

    /// New-machine guard: a fresh (absent) database next to a populated
    /// metadata snapshot must NOT trigger the post-v7 flush — flushing an
    /// empty DB would erase the snapshot. Instead the reindex imports the
    /// snapshot, toggles included, into the new database.
    #[test]
    fn fresh_db_with_existing_snapshot_is_imported_not_wiped() {
        let _lock = central_repo::test_base_dir_lock();
        let tmp = tempdir().unwrap();
        let base = tmp.path().join("repo");
        central_repo::set_test_base_dir_override(Some(base.clone()));

        seed_repo_with_stale_toggles(&base);

        // Simulate the re-clone-on-a-new-machine state: metadata + skills on
        // disk, no local database.
        for suffix in ["", "-wal", "-shm"] {
            let _ = fs::remove_file(base.join(format!("skills-manager.db{suffix}")));
        }

        let store = initialize_cli_store().expect("startup must succeed");

        let skills = store.get_all_skills().unwrap();
        assert_eq!(skills.len(), 1, "snapshot must be imported into the fresh DB");
        assert_eq!(skills[0].id, "skill-1");
        assert!(
            membership_tool_x("A") && membership_tool_x("B"),
            "metadata JSON must survive a fresh-DB startup untouched"
        );
        assert!(
            store
                .get_enabled_tools_for_scenario_skill("B", "skill-1")
                .unwrap()
                .contains(&"tool_x".to_string()),
            "imported membership must carry the snapshot's toggles"
        );

        central_repo::set_test_base_dir_override(None);
    }
}
