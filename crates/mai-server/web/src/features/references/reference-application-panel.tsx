/**
 * 项目参考源的应用状态面板。
 *
 * 明确区分"配置已保存"与"全部已应用"：只有服务端聚合状态为 `applied` 时才显示全部
 * 生效。等待、失败、延迟与"没有执行环境"分别展示，且计数来自完整集合而非当前分页。
 */

import { ChevronLeft, ChevronRight, RefreshCw, ShieldAlert, Zap } from "lucide-react"

import type { ProjectReferenceStatus, ReferenceApplicationState } from "@/api/reference-types"
import { StatusBadge } from "@/components/status"
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "@/components/ui/alert-dialog"
import { Button } from "@/components/ui/button"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"

const STATE_LABELS: Record<ReferenceApplicationState, string> = {
  applied: "Applied",
  pending: "Pending",
  applying: "Switching",
  waiting_idle: "Waiting for current turn",
  failed: "Failed",
  deferred: "Deferred",
  no_execution_environment: "No execution environment",
}

/** 应用状态的中文说明；同时用于告警文案。 */
export function applicationStateLabel(state: ReferenceApplicationState): string {
  return STATE_LABELS[state]
}

export function ReferenceApplicationPanel({
  status,
  onPageChange,
  onRetry,
  onStopAndApply,
  retryPending,
  stopAndApplyPending,
}: {
  status: ProjectReferenceStatus
  onPageChange(offset: number): void
  onRetry(): void
  onStopAndApply(): void
  retryPending: boolean
  stopAndApplyPending: boolean
}) {
  const { counts, page } = status
  const pendingCount = counts.waiting + counts.failed + counts.deferred
  const allApplied = status.state === "applied" && pendingCount === 0
  const pageStart = page.total === 0 ? 0 : page.offset + 1
  const pageEnd = Math.min(page.offset + page.limit, page.total)
  const busy = retryPending || stopAndApplyPending

  return (
    <section className="flex flex-col gap-4 rounded-lg border p-5" aria-label="Reference application status">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h2 className="text-sm font-semibold">Application</h2>
          <p className="mt-1 text-xs text-muted-foreground">
            Configuration revision {status.configuration.revision} · target revision {status.target_revision} ·
            observed {new Date(status.observed_at).toLocaleString()}
          </p>
        </div>
        <StatusBadge status={allApplied ? "applied" : status.state} />
      </div>

      <p className="text-sm" data-testid="reference-application-summary">
        {allApplied
          ? `All ${counts.applied} agent environments match revision ${status.target_revision}.`
          : `${counts.applied} applied · ${counts.waiting} waiting · ${counts.failed} failed · ${counts.deferred} deferred · ${counts.no_environment} without an environment.`}
      </p>

      {pendingCount > 0 && (
        <div className="flex items-start gap-2 rounded-lg border border-dashed p-3 text-xs text-muted-foreground">
          <ShieldAlert className="mt-0.5 size-4 shrink-0" />
          <p>
            The saved configuration is durable, but some environments have not converged yet. Waiting agents apply
            before their next turn; failed or deferred agents need an explicit retry.
          </p>
        </div>
      )}

      <div className="flex flex-wrap gap-2">
        <Button type="button" variant="outline" size="sm" disabled={busy} onClick={onRetry}>
          <RefreshCw data-icon="inline-start" /> {retryPending ? "Queueing…" : "Apply to waiting agents"}
        </Button>
        <AlertDialog>
          <AlertDialogTrigger asChild>
            <Button type="button" variant="destructive" size="sm" disabled={busy}>
              <Zap data-icon="inline-start" /> Stop tasks and apply now
            </Button>
          </AlertDialogTrigger>
          <AlertDialogContent>
            <AlertDialogHeader>
              <AlertDialogTitle>Stop running tasks and apply references?</AlertDialogTitle>
              <AlertDialogDescription>
                {pendingCount} agent environments are still on an older revision. The current turn is interrupted and
                in-flight process state is not preserved. Installed software and the workspace survive because the
                environment is snapshotted before switching. Agents that fail to switch stay blocked instead of falling
                back to the old sources.
              </AlertDialogDescription>
            </AlertDialogHeader>
            <AlertDialogFooter>
              <AlertDialogCancel>Cancel</AlertDialogCancel>
              <AlertDialogAction
                variant="destructive"
                aria-label="Confirm stop and apply"
                onClick={onStopAndApply}
              >
                Stop and apply
              </AlertDialogAction>
            </AlertDialogFooter>
          </AlertDialogContent>
        </AlertDialog>
      </div>

      <div className="overflow-hidden rounded-lg border">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Agent</TableHead>
              <TableHead>Applied revision</TableHead>
              <TableHead>State</TableHead>
              <TableHead>Phase / detail</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {status.agents.length === 0 ? (
              <TableRow>
                <TableCell colSpan={4} className="text-center text-xs text-muted-foreground">
                  No agents need this configuration.
                </TableCell>
              </TableRow>
            ) : (
              status.agents.map((agent) => (
                <TableRow key={agent.agent_id}>
                  <TableCell className="font-mono text-xs">{agent.agent_id.slice(0, 8)}</TableCell>
                  <TableCell className="text-xs">{agent.applied_revision}</TableCell>
                  <TableCell>
                    <StatusBadge status={agent.state} />
                  </TableCell>
                  <TableCell className="text-xs text-muted-foreground">
                    {agent.failure?.message ?? (agent.operation ? agent.operation.phase : STATE_LABELS[agent.state])}
                  </TableCell>
                </TableRow>
              ))
            )}
          </TableBody>
        </Table>
      </div>

      <nav className="flex items-center justify-between gap-3" aria-label="Reference application pages">
        <p className="text-xs text-muted-foreground">
          {pageStart}–{pageEnd} of {page.total}
        </p>
        <div className="flex gap-2">
          <Button
            type="button"
            variant="outline"
            size="sm"
            aria-label="Previous agents"
            disabled={page.offset === 0}
            onClick={() => onPageChange(Math.max(0, page.offset - page.limit))}
          >
            <ChevronLeft data-icon="inline-start" /> Previous
          </Button>
          <Button
            type="button"
            variant="outline"
            size="sm"
            aria-label="Next agents"
            disabled={pageEnd >= page.total}
            onClick={() => onPageChange(page.offset + page.limit)}
          >
            Next <ChevronRight data-icon="inline-end" />
          </Button>
        </div>
      </nav>
    </section>
  )
}
