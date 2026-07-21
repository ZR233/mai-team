import { AlertCircle, CheckCircle2, LoaderCircle, Wifi, WifiOff } from "lucide-react"

import { Alert, AlertDescription } from "@/components/ui/alert"
import { Badge } from "@/components/ui/badge"
import { cn } from "@/lib/utils"

const healthy = new Set(["ready", "active", "completed", "idle", "live"])
const pending = new Set(["provisioning", "queued", "running", "streaming", "connecting", "resyncing"])
const failed = new Set(["failed", "error", "errored", "faulted", "offline", "cancelled"])

export function StatusBadge({ status, className }: { status?: string | null; className?: string }) {
  const value = status || "unknown"
  const variant = failed.has(value) ? "destructive" : pending.has(value) ? "secondary" : healthy.has(value) ? "outline" : "secondary"
  return <Badge variant={variant} className={cn("capitalize", className)}>{value}</Badge>
}

export function StatusDot({ status }: { status?: string | null }) {
  const value = status || "unknown"
  return <span className={cn(
    "inline-block size-2 rounded-full bg-muted-foreground",
    healthy.has(value) && "bg-foreground",
    pending.has(value) && "animate-pulse bg-primary",
    failed.has(value) && "bg-destructive",
  )} />
}

export function ConnectionStatus({ status, message }: { status: string; message?: string | null }) {
  const live = status === "live"
  const pendingState = status === "connecting" || status === "resyncing"
  return (
    <div className={cn("flex items-center gap-2 text-xs text-muted-foreground", live && "text-foreground")}>
      {live ? <Wifi className="size-3.5" /> : pendingState ? <LoaderCircle className="size-3.5 animate-spin" /> : <WifiOff className="size-3.5" />}
      <span>{message || (live ? "Connected" : pendingState ? "Connecting" : "Offline")}</span>
    </div>
  )
}

export function InlineNotice({ children, tone = "neutral" }: { children: React.ReactNode; tone?: "neutral" | "error" | "success" }) {
  const Icon = tone === "error" ? AlertCircle : CheckCircle2
  return (
    <Alert variant={tone === "error" ? "destructive" : "default"}>
      {tone !== "neutral" && <Icon />}
      <AlertDescription>{children}</AlertDescription>
    </Alert>
  )
}
