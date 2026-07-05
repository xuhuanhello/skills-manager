import { useMemo, useState } from "react";
import { createPortal } from "react-dom";
import { Loader2, Globe2, X } from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { cn } from "../utils";
import * as api from "../lib/tauri";
import type { ManagedSkill, ToolInfo } from "../lib/tauri";
import { getErrorMessage } from "../lib/error";
import { AgentIcon } from "./AgentIcon";

export interface BatchSyncAgentsSheetProps {
  open: boolean;
  onClose: () => void;
  /** Skills the wizard operates on (already user-selected in the list). */
  skills: ManagedSkill[];
  /** Eligible global agent tools (installed + enabled, same category as the
   *  current workspace). The caller scopes this so the wizard only shows
   *  agents the user can actually target. */
  tools: ToolInfo[];
  onApplied: () => void | Promise<void>;
}

/**
 * Batch "set which global agents the selected skills sync to" wizard.
 *
 * Replaces the one-by-one workflow of opening each skill's detail panel and
 * toggling its agent checkboxes. The user selects N skills in the MySkills
 * multiselect (or the agent-detail view), opens this sheet, picks a target
 * set of global agents, and a single `batch_apply_skills` round-trip adds
 * and a single round-trip removes — driving the same `scenario_service`
 * primitive the tray uses, instead of the old per-(skill,agent) IPC loop.
 */
export function BatchSyncAgentsSheet({
  open,
  onClose,
  skills,
  tools,
  onApplied,
}: BatchSyncAgentsSheetProps) {
  const { t } = useTranslation();
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [applying, setApplying] = useState(false);

  // Aggregated current sync state per agent across the selected skills,
  // derived purely from each ManagedSkill's `targets` list.
  const currentByAgent = useMemo(() => {
    const map = new Map<string, number>();
    for (const skill of skills) {
      for (const target of skill.targets) {
        map.set(target.tool, (map.get(target.tool) ?? 0) + 1);
      }
    }
    return map;
  }, [skills]);

  const skillCount = skills.length;

  const eligibleTools = useMemo(
    () => tools.filter((tool) => tool.installed && tool.enabled),
    [tools],
  );

  const toggleAgent = (key: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  };

  const selectAll = () => setSelected(new Set(eligibleTools.map((t) => t.key)));
  const clearAll = () => setSelected(new Set());

  const toAdd: string[] = [];
  const toRemove: string[] = [];
  for (const tool of eligibleTools) {
    const want = selected.has(tool.key);
    const hasAny = (currentByAgent.get(tool.key) ?? 0) > 0;
    if (want && !hasAny) toAdd.push(tool.key);
    else if (!want && hasAny) toRemove.push(tool.key);
  }
  const skillIds = useMemo(() => skills.map((s) => s.id), [skills]);
  const dirty = toAdd.length > 0 || toRemove.length > 0;

  const handleApply = async () => {
    if (skillIds.length === 0 || !dirty) return;
    setApplying(true);
    let failedAdd = 0;
    let failedRemove = 0;
    try {
      if (toAdd.length > 0) {
        const res = await api.batchApplySkills(skillIds, toAdd, "add");
        failedAdd = res.failed;
      }
      if (toRemove.length > 0) {
        const res = await api.batchApplySkills(skillIds, toRemove, "remove");
        failedRemove = res.failed;
      }
      const totalFailed = failedAdd + failedRemove;
      if (totalFailed === 0) {
        toast.success(
          t("mySkills.batchSyncAgents.applied", {
            added: toAdd.length,
            removed: toRemove.length,
          }),
        );
      } else {
        toast.error(
          t("mySkills.batchSyncAgents.partialFailed", { count: totalFailed }),
        );
      }
      await onApplied();
      onClose();
      setSelected(new Set());
    } catch (error) {
      toast.error(getErrorMessage(error, t("common.error")));
    } finally {
      setApplying(false);
    }
  };

  if (!open) return null;

  return createPortal(
    <div className="fixed inset-0 z-50">
      <div
        className="absolute inset-0 bg-black/40 backdrop-blur-[1px]"
        onClick={() => !applying && onClose()}
      />
      <div className="absolute right-0 top-0 flex h-full w-full max-w-[460px] flex-col overflow-hidden border-l border-border-subtle bg-bg-secondary shadow-2xl">
        {/* Header */}
        <div className="flex shrink-0 items-start justify-between gap-3 border-b border-border-subtle px-5 py-4">
          <div className="min-w-0 flex-1">
            <h2 className="flex items-center gap-2 text-[14px] font-semibold text-primary">
              <Globe2 className="h-4 w-4 text-accent" />
              {t("mySkills.batchSyncAgents.title")}
            </h2>
            <p className="mt-1.5 text-[12.5px] text-muted">
              {t("mySkills.batchSyncAgents.subtitle", { count: skillCount })}
            </p>
          </div>
          <button
            onClick={onClose}
            disabled={applying}
            className="shrink-0 rounded-[4px] p-1.5 text-muted transition-colors hover:bg-surface-hover hover:text-secondary disabled:opacity-50"
          >
            <X className="h-4 w-4" />
          </button>
        </div>

        {/* Agent list */}
        <div className="min-h-0 flex-1 overflow-y-auto px-5 py-4">
          <div className="mb-2.5 flex items-center justify-between">
            <span className="text-[12px] text-muted">
              {t("mySkills.batchSyncAgents.agentsHint")}
            </span>
            <div className="flex items-center gap-1.5">
              <button
                onClick={selectAll}
                disabled={applying || eligibleTools.length === 0}
                className="rounded-md px-2 py-1 text-[12px] font-medium text-muted transition-colors hover:bg-surface-hover hover:text-secondary disabled:opacity-50"
              >
                {t("mySkills.selectAll")}
              </button>
              <button
                onClick={clearAll}
                disabled={applying || selected.size === 0}
                className="rounded-md px-2 py-1 text-[12px] font-medium text-muted transition-colors hover:bg-surface-hover hover:text-secondary disabled:opacity-50"
              >
                {t("mySkills.deselectAll")}
              </button>
            </div>
          </div>

          {eligibleTools.length === 0 ? (
            <div className="py-6 text-center text-[13px] text-muted">
              {t("mySkills.batchSyncAgents.noAgents")}
            </div>
          ) : (
            <div className="space-y-1.5">
              {eligibleTools.map((tool) => {
                const checked = selected.has(tool.key);
                const syncedCount = currentByAgent.get(tool.key) ?? 0;
                const fullySynced = syncedCount === skillCount && skillCount > 0;
                const partial = syncedCount > 0 && !fullySynced;
                return (
                  <button
                    key={tool.key}
                    onClick={() => !applying && toggleAgent(tool.key)}
                    disabled={applying}
                    className={cn(
                      "flex w-full items-center gap-3 rounded-xl border px-3.5 py-2.5 text-left transition-colors disabled:opacity-60",
                      checked
                        ? "border-accent/70 bg-accent/8"
                        : "border-border-subtle bg-surface hover:bg-surface-hover",
                    )}
                  >
                    <span
                      className={cn(
                        "flex h-4 w-4 shrink-0 items-center justify-center rounded-[4px] border transition-colors",
                        checked
                          ? "border-accent bg-accent text-white"
                          : "border-border-subtle bg-bg-secondary",
                      )}
                    >
                      {checked && (
                        <svg viewBox="0 0 12 12" className="h-3 w-3" fill="none">
                          <path
                            d="M2.5 6.5l2.5 2.5 4.5-5"
                            stroke="currentColor"
                            strokeWidth="2"
                            strokeLinecap="round"
                            strokeLinejoin="round"
                          />
                        </svg>
                      )}
                    </span>
                    <AgentIcon
                      agentKey={tool.key}
                      displayName={tool.display_name}
                      className="h-6 w-6 rounded-md"
                    />
                    <span className="min-w-0 flex-1 truncate text-[13px] font-medium text-secondary">
                      {tool.display_name}
                    </span>
                    <span
                      className={cn(
                        "shrink-0 rounded-full px-2 py-0.5 text-[11px] font-medium",
                        fullySynced
                          ? "bg-emerald-500/12 text-emerald-600 dark:text-emerald-400"
                          : partial
                          ? "bg-amber-500/12 text-amber-600 dark:text-amber-400"
                          : "bg-surface-hover text-muted",
                      )}
                    >
                      {t("mySkills.batchSyncAgents.syncedCount", {
                        synced: syncedCount,
                        total: skillCount,
                      })}
                    </span>
                  </button>
                );
              })}
            </div>
          )}
        </div>

        {/* Footer */}
        <div className="shrink-0 border-t border-border-subtle px-5 py-3.5">
          <div className="mb-2.5 flex flex-wrap items-center gap-2 text-[12px] text-muted">
            {toAdd.length > 0 && (
              <span className="rounded-full bg-emerald-500/12 px-2 py-0.5 text-emerald-600 dark:text-emerald-400">
                {t("mySkills.batchSyncAgents.willAdd", { count: toAdd.length })}
              </span>
            )}
            {toRemove.length > 0 && (
              <span className="rounded-full bg-amber-500/12 px-2 py-0.5 text-amber-600 dark:text-amber-400">
                {t("mySkills.batchSyncAgents.willRemove", { count: toRemove.length })}
              </span>
            )}
            {toAdd.length === 0 && toRemove.length === 0 && (
              <span>{t("mySkills.batchSyncAgents.noChange")}</span>
            )}
          </div>
          <div className="flex items-center justify-end gap-2">
            <button
              onClick={onClose}
              disabled={applying}
              className="rounded-md px-3 py-1.5 text-[13px] font-medium text-muted transition-colors hover:bg-surface-hover hover:text-secondary disabled:opacity-50"
            >
              {t("common.cancel")}
            </button>
            <button
              onClick={handleApply}
              disabled={applying || !dirty || skillIds.length === 0}
              className="inline-flex items-center gap-1.5 rounded-md bg-accent px-3.5 py-1.5 text-[13px] font-medium text-white transition-colors hover:bg-accent-hover disabled:opacity-50"
            >
              {applying && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
              {t("mySkills.batchSyncAgents.apply")}
            </button>
          </div>
        </div>
      </div>
    </div>,
    document.body,
  );
}