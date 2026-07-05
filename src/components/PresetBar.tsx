import { useCallback, useMemo, useState } from "react";
import { Check, Loader2 } from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { cn } from "../utils";
import { computePresetStatus } from "../lib/presetStatus";
import { getPresetIconOption } from "../lib/presetIcons";
import type { BatchApplyResult, ManagedSkill, Preset } from "../lib/tauri";
import { getErrorMessage } from "../lib/error";

export interface PresetBarProps {
  presets: Preset[];
  managedSkills: ManagedSkill[];
  agentKeys: string[];
  existsInWorkspace: (skill: ManagedSkill, agentKey: string) => boolean;
  /** Per-(skill, agent) add. Used as the slow fallback when `onBatchApply` is
   * not provided (notably ProjectDetail, whose project-scoped export/delete
   * primitives have no batch equivalent today). */
  onAddSkill: (skill: ManagedSkill, agentKey: string) => Promise<void>;
  /** Per-(skill, agent) remove — same fallback role as `onAddSkill`. */
  onRemoveSkill: (skill: ManagedSkill, agentKey: string) => Promise<void>;
  /** One-shot batch backend for adding/removing a set of skills against a set
   * of agent tool keys. When provided, PresetBar issues a single IPC
   * round-trip per preset toggle instead of looping O(skills × agents)
   * `await`s — this is what fixes the "enable/disable hangs" symptom in the
   * global/single-agent workspace views. */
  onBatchApply?: (
    skillIds: string[],
    toolKeys: string[],
    mode: "add" | "remove",
  ) => Promise<BatchApplyResult>;
  onComplete: () => Promise<void>;
}

export function PresetBar({
  presets,
  managedSkills,
  agentKeys,
  existsInWorkspace,
  onAddSkill,
  onRemoveSkill,
  onBatchApply,
  onComplete,
}: PresetBarProps) {
  const { t } = useTranslation();
  const [loadingKey, setLoadingKey] = useState<string | null>(null);

  const statuses = useMemo(() => {
    const map = new Map<string, ReturnType<typeof computePresetStatus>>();
    for (const preset of presets) {
      map.set(preset.id, computePresetStatus(preset, managedSkills, agentKeys, existsInWorkspace));
    }
    return map;
  }, [presets, managedSkills, agentKeys, existsInWorkspace]);

  const visiblePresets = useMemo(
    () => presets.filter((p) => statuses.get(p.id)?.status !== "empty"),
    [presets, statuses]
  );

  const handleActivate = useCallback(async (preset: Preset) => {
    setLoadingKey(`${preset.id}-add`);
    try {
      const presetSkills = managedSkills.filter((s) => s.preset_ids.includes(preset.id));
      if (onBatchApply && agentKeys.length > 0) {
        const skillIds = presetSkills.map((s) => s.id);
        // Pairs already synced contribute to `skipped` in the toast; backend
        // applies the whole cartesian product (insert_target upserts, so
        // re-syncing an existing pair is idempotent and harmless).
        const attemptedNew = presetSkills.reduce(
          (n, s) => n + agentKeys.filter((k) => !existsInWorkspace(s, k)).length,
          0,
        );
        const totalPairs = presetSkills.length * agentKeys.length;
        let failed = 0;
        if (skillIds.length > 0 && attemptedNew > 0) {
          const res = await onBatchApply(skillIds, agentKeys, "add");
          failed = res.failed;
          const added = Math.max(0, attemptedNew - failed);
          const skipped = totalPairs - attemptedNew;
          if (added > 0) toast.success(t("presetActions.addedToast", { added, skipped }));
          else if (failed === 0) toast.info(t("presetActions.nothingToAdd"));
          if (failed > 0) toast.error(t("presetActions.partialFailedToast", { count: failed }));
        } else {
          toast.info(t("presetActions.nothingToAdd"));
        }
      } else {
        let added = 0, skipped = 0, failed = 0;
        for (const skill of presetSkills) {
          for (const agentKey of agentKeys) {
            if (existsInWorkspace(skill, agentKey)) { skipped++; continue; }
            try { await onAddSkill(skill, agentKey); added++; }
            catch { failed++; }
          }
        }
        if (added > 0) toast.success(t("presetActions.addedToast", { added, skipped }));
        else if (failed === 0) toast.info(t("presetActions.nothingToAdd"));
        if (failed > 0) toast.error(t("presetActions.partialFailedToast", { count: failed }));
      }
      await onComplete();
    } catch (error) {
      toast.error(getErrorMessage(error, t("common.error")));
    } finally {
      setLoadingKey(null);
    }
  }, [agentKeys, existsInWorkspace, managedSkills, onAddSkill, onBatchApply, onComplete, t]);

  const handleDeactivate = useCallback(async (preset: Preset) => {
    setLoadingKey(`${preset.id}-remove`);
    try {
      const presetSkills = managedSkills.filter((s) => s.preset_ids.includes(preset.id));
      if (onBatchApply && agentKeys.length > 0) {
        const removable = presetSkills.filter((s) => agentKeys.some((k) => existsInWorkspace(s, k)));
        const skillIds = removable.map((s) => s.id);
        let failed = 0;
        if (skillIds.length > 0) {
          const res = await onBatchApply(skillIds, agentKeys, "remove");
          failed = res.failed;
          const removed = Math.max(0, removable.length - failed);
          if (removed > 0) toast.success(t("presetActions.removedToast", { removed }));
          else if (failed === 0) toast.info(t("presetActions.nothingToRemove"));
          if (failed > 0) toast.error(t("presetActions.partialFailedToast", { count: failed }));
        } else {
          toast.info(t("presetActions.nothingToRemove"));
        }
      } else {
        let removed = 0, failed = 0;
        for (const skill of presetSkills) {
          for (const agentKey of agentKeys) {
            if (!existsInWorkspace(skill, agentKey)) continue;
            try { await onRemoveSkill(skill, agentKey); removed++; }
            catch { failed++; }
          }
        }
        if (removed > 0) toast.success(t("presetActions.removedToast", { removed }));
        else if (failed === 0) toast.info(t("presetActions.nothingToRemove"));
        if (failed > 0) toast.error(t("presetActions.partialFailedToast", { count: failed }));
      }
      await onComplete();
    } catch (error) {
      toast.error(getErrorMessage(error, t("common.error")));
    } finally {
      setLoadingKey(null);
    }
  }, [agentKeys, existsInWorkspace, managedSkills, onBatchApply, onComplete, onRemoveSkill, t]);

  if (visiblePresets.length === 0) return null;

  const busy = loadingKey !== null;

  return (
    <div className="flex min-w-0 flex-wrap items-center gap-1.5">
      <span className="shrink-0 text-[12px] text-muted">{t("sidebar.presets")}</span>
      <div className="flex min-w-0 flex-1 items-center gap-1.5 overflow-x-auto scrollbar-hide">
        {visiblePresets.map((preset) => {
          const s = statuses.get(preset.id)!;
          const presetIcon = getPresetIconOption(preset);
          const Icon = presetIcon.icon;
          const isLoading = loadingKey?.startsWith(preset.id) ?? false;

          return (
            <button
              key={preset.id}
              onClick={() => {
                if (busy) return;
                if (s.status === "active") handleDeactivate(preset);
                else handleActivate(preset);
              }}
              disabled={busy}
              title={preset.name}
              className={cn(
                "inline-flex shrink-0 items-center gap-1 rounded-full border px-2.5 py-0.5 text-[12px] font-medium transition-colors disabled:opacity-50",
                s.status === "active"
                  ? `${presetIcon.activeClass} ${presetIcon.colorClass}`
                  : s.status === "partial"
                  ? "border-amber-400/50 bg-amber-500/8 text-amber-600 dark:text-amber-400 hover:bg-amber-500/12"
                  : "border-border-subtle text-faint hover:border-border hover:text-muted"
              )}
            >
              {isLoading
                ? <Loader2 className="h-3 w-3 animate-spin" />
                : <Icon className="h-3 w-3" />}
              <span className="max-w-[140px] truncate">{preset.name}</span>
              {s.status === "active" && <Check className="h-3 w-3 shrink-0" />}
              {s.status === "partial" && (
                <span className="rounded-full bg-amber-500/20 px-1.5 py-px text-[10px] font-semibold">
                  {s.installed}/{s.total}
                </span>
              )}
            </button>
          );
        })}
      </div>
    </div>
  );
}
