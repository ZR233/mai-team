import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query"
import { ExternalLink, Github, RefreshCw, RotateCcw, Upload } from "lucide-react"
import { useEffect, useState } from "react"
import { toast } from "sonner"

import { api, jsonBody } from "@/api/client"
import type { GithubAppSettings, RelaySettings, RelayStatus } from "@/api/product-types"
import { queryKeys } from "@/api/queries"
import { ErrorState, LoadingState } from "@/components/page-state"
import { StatusBadge } from "@/components/status"
import { Alert, AlertDescription } from "@/components/ui/alert"
import { Avatar, AvatarFallback } from "@/components/ui/avatar"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Field, FieldContent, FieldDescription, FieldGroup, FieldTitle } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { Switch } from "@/components/ui/switch"
import { Textarea } from "@/components/ui/textarea"
import {
  buildGithubAppSettingsUpdate,
  buildRelaySettingsUpdate,
} from "@/features/settings/sections/github-app-settings-payload"
import { FormField, SettingsBody, SettingsHeader } from "@/features/settings/settings-section"

interface Installation {
  id: number
  account_login: string
  account_type: string
  repository_selection?: string | null
  events?: string[]
}

interface UpdateStatus {
  current_version: string
  latest_version: string
  has_update: boolean
  can_update: boolean
  warning?: string | null
  restart_scheduled?: boolean
}

export function GithubAppSection() {
  const queryClient = useQueryClient()
  const relay = useQuery({ queryKey: queryKeys.relay, queryFn: () => api<RelayStatus>("/relay/status") })
  const relaySettings = useQuery({ queryKey: [...queryKeys.relay, "settings"], queryFn: () => api<RelaySettings>("/settings/relay") })
  const app = useQuery({ queryKey: queryKeys.githubApp, queryFn: () => api<GithubAppSettings>("/settings/github-app") })
  const installations = useQuery({ queryKey: [...queryKeys.githubApp, "installations"], queryFn: () => api<{ installations: Installation[] }>("/github/installations") })
  const update = useQuery({ queryKey: [...queryKeys.relay, "update"], queryFn: () => api<UpdateStatus>("/relay/update") })
  const [relayForm, setRelayForm] = useState({ enabled: false, url: "", token: "", nodeId: "mai-server" })
  const [appForm, setAppForm] = useState({ publicUrl: "", baseUrl: "", appId: "", privateKey: "" })

  useEffect(() => {
    if (relaySettings.data) {
      setRelayForm((current) => ({ ...current, enabled: relaySettings.data.enabled, url: relaySettings.data.url, nodeId: relaySettings.data.node_id }))
    }
  }, [relaySettings.data])
  useEffect(() => {
    if (app.data) {
      setAppForm((current) => ({ ...current, publicUrl: app.data.public_url || "", baseUrl: app.data.base_url || "", appId: app.data.app_id || "", privateKey: "" }))
    }
  }, [app.data])

  const refresh = async () => {
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: queryKeys.relay }),
      queryClient.invalidateQueries({ queryKey: queryKeys.githubApp }),
    ])
  }
  const saveRelay = useMutation({
    mutationFn: () => api<RelaySettings>("/settings/relay", { method: "PUT", ...jsonBody(buildRelaySettingsUpdate(relayForm)) }),
    onSuccess: async () => {
      toast.success("Relay settings saved")
      setRelayForm((current) => ({ ...current, token: "" }))
      await refresh()
    },
    onError: (error) => toast.error(error.message),
  })
  const saveApp = useMutation({
    mutationFn: () => api<GithubAppSettings>("/settings/github-app", { method: "PUT", ...jsonBody(buildGithubAppSettingsUpdate(appForm)) }),
    onSuccess: async () => {
      toast.success("GitHub App settings saved")
      setAppForm((current) => ({ ...current, privateKey: "" }))
      await refresh()
    },
    onError: (error) => toast.error(error.message),
  })
  const refreshIdentity = useMutation({
    mutationFn: () => api<GithubAppSettings>("/settings/github-app/refresh", { method: "POST" }),
    onSuccess: (settings) => {
      queryClient.setQueryData(queryKeys.githubApp, settings)
      toast.success("GitHub identity refreshed")
    },
    onError: (error) => toast.error(error.message),
  })

  if ([relay, relaySettings, app, installations, update].some((item) => item.isLoading)) return <LoadingState rows={7} />
  const firstError = [relay, relaySettings, app, installations, update].find((item) => item.error)?.error
  if (firstError) return <ErrorState error={firstError} retry={() => void refresh()} />

  const install = async () => {
    try {
      const result = await api<{ install_url: string }>("/github/app-installation/start", { method: "POST", ...jsonBody({ origin: window.location.origin, return_hash: "/settings/github-app" }) })
      window.location.assign(result.install_url)
    } catch (error) {
      toast.error(error instanceof Error ? error.message : String(error))
    }
  }
  const updateAction = async (action: "check" | "apply" | "rollback") => {
    try {
      await api(`/relay/update:${action}`, { method: "POST", ...(action === "check" ? jsonBody({ force: true }) : {}) })
      toast.success(`Relay ${action} requested`)
      await refresh()
    } catch (error) {
      toast.error(error instanceof Error ? error.message : String(error))
    }
  }
  const refreshInstallations = async () => {
    try {
      await api("/github/installations:refresh", { method: "POST" })
      await installations.refetch()
      toast.success("Installations refreshed")
    } catch (error) {
      toast.error(error instanceof Error ? error.message : String(error))
    }
  }

  return (
    <>
      <SettingsHeader title="GitHub App" description="Connect mai-relay and manage relay-owned GitHub App installations." status={<StatusBadge status={relay.data?.connected ? "connected" : "unavailable"} />} />
      <SettingsBody>
        <div className="mb-5 grid gap-3 sm:grid-cols-3">
          <Metric label="Relay" value={relay.data?.connected ? "Connected" : "Unavailable"} detail={relay.data?.relay_url || relay.data?.message || "Not configured"} status={relay.data?.connected ? "connected" : "unavailable"} />
          <Metric label="Application" value={app.data?.github_name || app.data?.app_slug || "Not configured"} detail={app.data?.owner_login || app.data?.base_url || "Relay owns credentials"} status={app.data?.bot_user_id ? "configured" : "incomplete"} />
          <Metric label="Installations" value={String(installations.data?.installations.length ?? 0)} detail="Repository access grants" status={installations.data?.installations.length ? "ready" : "empty"} />
        </div>

        <div className="grid gap-5 xl:grid-cols-2">
          <section className="rounded-lg border p-5">
            <h2 className="mb-4 text-sm font-semibold">Relay connection</h2>
            <FieldGroup>
              <Field orientation="horizontal" className="rounded-lg border bg-muted/40 p-3">
                <FieldContent><FieldTitle>Enable relay</FieldTitle><FieldDescription>Connect this server to the configured mai-relay endpoint.</FieldDescription></FieldContent>
                <Switch aria-label="Enable relay" checked={relayForm.enabled} onCheckedChange={(enabled: boolean) => setRelayForm({ ...relayForm, enabled })} />
              </Field>
              <FormField label="Relay URL"><Input value={relayForm.url} onChange={(event) => setRelayForm({ ...relayForm, url: event.target.value })} placeholder="https://relay.example" /></FormField>
              <FormField label="Relay token" htmlFor="relay-token" hint={relaySettings.data?.has_token ? "Leave blank to preserve the saved token." : undefined}><Input id="relay-token" type="password" value={relayForm.token} onChange={(event) => setRelayForm({ ...relayForm, token: event.target.value })} /></FormField>
              <FormField label="Node ID"><Input value={relayForm.nodeId} onChange={(event) => setRelayForm({ ...relayForm, nodeId: event.target.value })} /></FormField>
              <Button disabled={saveRelay.isPending} onClick={() => saveRelay.mutate()}>Save relay</Button>
            </FieldGroup>
          </section>

          <section className="rounded-lg border p-5">
            <h2 className="mb-4 text-sm font-semibold">GitHub App projection</h2>
            <FieldGroup>
              <FormField label="Public URL"><Input value={appForm.publicUrl} onChange={(event) => setAppForm({ ...appForm, publicUrl: event.target.value })} placeholder="https://relay.example" /></FormField>
              <FormField label="GitHub API base URL"><Input value={appForm.baseUrl} onChange={(event) => setAppForm({ ...appForm, baseUrl: event.target.value })} /></FormField>
              <FormField label="GitHub App ID"><Input value={appForm.appId} onChange={(event) => setAppForm({ ...appForm, appId: event.target.value })} placeholder="123456" /></FormField>
              <FormField label="PEM private key" htmlFor="github-app-private-key" hint={app.data?.has_private_key ? "Leave blank to preserve the saved private key." : "Paste or upload the GitHub App private key."}><Textarea id="github-app-private-key" className="min-h-24 font-mono text-xs" value={appForm.privateKey} onChange={(event) => setAppForm({ ...appForm, privateKey: event.target.value })} /></FormField>
              <FormField label="Upload PEM"><Input type="file" accept=".pem,.key,text/plain" onChange={(event) => { const file = event.target.files?.[0]; if (!file) return; const reader = new FileReader(); reader.onload = () => setAppForm((current) => ({ ...current, privateKey: String(reader.result || "") })); reader.readAsText(file); event.target.value = "" }} /></FormField>
              <div className="flex gap-2"><Button disabled={saveApp.isPending} onClick={() => saveApp.mutate()}>Save app</Button><Button variant="outline" disabled={!relay.data?.connected || !app.data?.install_url} onClick={() => void install()}><ExternalLink data-icon="inline-start" /> Install app</Button></div>
            </FieldGroup>
          </section>
        </div>

        <VerifiedIdentity
          settings={app.data}
          connected={Boolean(relay.data?.connected)}
          refreshing={refreshIdentity.isPending}
          onRefresh={() => refreshIdentity.mutate()}
        />

        <section className="mt-5 overflow-hidden rounded-lg border">
          <div className="flex items-center justify-between border-b px-4 py-3">
            <div><h2 className="text-sm font-semibold">Relay update</h2><p className="text-xs text-muted-foreground">{update.data?.current_version || "unknown"} → {update.data?.latest_version || "unknown"}</p></div>
            {update.data?.has_update ? <Badge>Update available</Badge> : <Badge variant="outline">Current</Badge>}
          </div>
          <div className="flex flex-wrap gap-2 p-4">
            <Button variant="outline" onClick={() => void updateAction("check")}><RefreshCw data-icon="inline-start" /> Check</Button>
            <Button disabled={!update.data?.can_update || !update.data?.has_update} onClick={() => void updateAction("apply")}><Upload data-icon="inline-start" /> Apply</Button>
            <Button variant="outline" onClick={() => void updateAction("rollback")}><RotateCcw data-icon="inline-start" /> Roll back</Button>
            {update.data?.warning && <Alert className="mt-2 basis-full"><AlertDescription>{update.data.warning}</AlertDescription></Alert>}
          </div>
        </section>

        <section className="mt-5 overflow-hidden rounded-lg border">
          <div className="flex items-center justify-between border-b px-4 py-3"><h2 className="text-sm font-semibold">Installations</h2><Button variant="ghost" size="sm" onClick={() => void refreshInstallations()}><RefreshCw data-icon="inline-start" /> Refresh</Button></div>
          <div className="divide-y">
            {installations.data?.installations.map((item) => <div key={item.id} className="flex items-center gap-3 px-4 py-3">
              <Avatar className="size-9 rounded-lg"><AvatarFallback className="rounded-lg"><Github className="size-4" /></AvatarFallback></Avatar>
              <div className="min-w-0 flex-1"><strong className="block truncate text-sm">{item.account_login}</strong><p className="truncate text-xs text-muted-foreground">{item.account_type} · {item.repository_selection || "all repositories"}</p></div>
              <StatusBadge status={item.events?.includes("pull_request") ? "ready" : "incomplete"} />
              <code className="text-xs text-muted-foreground">#{item.id}</code>
            </div>)}
          </div>
        </section>
      </SettingsBody>
    </>
  )
}

function VerifiedIdentity({ settings, connected, refreshing, onRefresh }: { settings?: GithubAppSettings; connected: boolean; refreshing: boolean; onRefresh(): void }) {
  const verified = Boolean(settings?.bot_user_id && settings.bot_login && settings.app_slug)
  return (
    <section className="mt-5 overflow-hidden rounded-lg border" aria-labelledby="verified-github-identity">
      <div className="flex flex-wrap items-center justify-between gap-3 border-b px-4 py-3">
        <div>
          <div className="flex items-center gap-2">
            <h2 id="verified-github-identity" className="text-sm font-semibold">Verified identity</h2>
            <Badge variant={verified ? "outline" : "secondary"}>{verified ? "Verified by GitHub" : "Refresh required"}</Badge>
          </div>
          <p className="mt-1 text-xs text-muted-foreground">Review authorship is matched by the immutable GitHub Bot user ID.</p>
        </div>
        <div className="flex flex-wrap gap-2">
          <Button variant="outline" disabled={!connected || refreshing} onClick={onRefresh}>
            <RefreshCw className={refreshing ? "animate-spin" : undefined} data-icon="inline-start" />
            Refresh identity
          </Button>
          {settings?.manage_url && <Button asChild variant="outline"><a href={settings.manage_url} target="_blank" rel="noreferrer"><ExternalLink data-icon="inline-start" /> Manage on GitHub</a></Button>}
        </div>
      </div>
      <dl className="grid gap-px bg-border sm:grid-cols-2 xl:grid-cols-5">
        <IdentityValue label="GitHub App name" value={settings?.github_name} />
        <IdentityValue label="Slug" value={settings?.app_slug} mono />
        <IdentityValue label="Bot login" value={settings?.bot_login} mono />
        <IdentityValue label="Bot user ID" value={settings?.bot_user_id ? String(settings.bot_user_id) : undefined} mono />
        <IdentityValue label="Owner" value={settings?.owner_login} detail={settings?.owner_type} className="sm:col-span-2 xl:col-span-1" />
      </dl>
    </section>
  )
}

function IdentityValue({ label, value, detail, mono = false, className = "" }: { label: string; value?: string | null; detail?: string | null; mono?: boolean; className?: string }) {
  return <div className={`min-w-0 bg-card px-4 py-3 ${className}`}><dt className="text-[11px] text-muted-foreground">{label}</dt><dd className={mono ? "mt-1 break-all font-mono text-xs" : "mt-1 truncate text-sm font-medium"}>{value || "Not verified"}</dd>{detail && <span className="mt-0.5 block text-xs text-muted-foreground">{detail}</span>}</div>
}

function Metric({ label, value, detail, status }: { label: string; value: string; detail: string; status: string }) {
  return <div className="rounded-lg border p-4"><div className="mb-2 flex items-center justify-between gap-2"><span className="text-xs text-muted-foreground">{label}</span><StatusBadge status={status} /></div><strong className="block truncate text-sm">{value}</strong><small className="mt-1 block truncate text-xs text-muted-foreground">{detail}</small></div>
}
