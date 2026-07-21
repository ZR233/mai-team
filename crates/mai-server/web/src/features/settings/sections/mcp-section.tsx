import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query"
import { Pencil, Plus, RefreshCw, ServerCog, Trash2 } from "lucide-react"
import { useState } from "react"
import { toast } from "sonner"

import { api, jsonBody } from "@/api/client"
import type { McpServerAggregate, McpServersResponse } from "@/api/product-types"
import { queryKeys } from "@/api/queries"
import { EmptyState, ErrorState, LoadingState } from "@/components/page-state"
import { StatusBadge } from "@/components/status"
import { Alert, AlertDescription } from "@/components/ui/alert"
import { AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent, AlertDialogDescription, AlertDialogFooter, AlertDialogHeader, AlertDialogTitle, AlertDialogTrigger } from "@/components/ui/alert-dialog"
import { Avatar, AvatarFallback } from "@/components/ui/avatar"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Field, FieldContent, FieldDescription, FieldGroup, FieldTitle } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectGroup, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Spinner } from "@/components/ui/spinner"
import { Switch } from "@/components/ui/switch"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { Textarea } from "@/components/ui/textarea"
import { FormField, SettingsBody, SettingsHeader } from "@/features/settings/settings-section"

interface McpForm {
  id: string
  scope: string
  transport: string
  command: string
  args: string
  cwd: string
  url: string
  env: string
  headers: string
  bearerToken: string
  bearerTokenEnv: string
  enabled: boolean
  required: boolean
}

export function McpSection() {
  const queryClient = useQueryClient()
  const query = useQuery({ queryKey: queryKeys.mcpServers, queryFn: () => api<McpServersResponse>("/mcp-servers") })
  const [editing, setEditing] = useState<McpServerAggregate | "new" | null>(null)
  const refresh = () => queryClient.invalidateQueries({ queryKey: queryKeys.mcpServers })
  const recheck = useMutation({
    mutationFn: () => api("/mcp-servers/recheck", { method: "POST" }),
    onSuccess: async () => {
      toast.success("MCP health rechecked")
      await refresh()
    },
    onError: (error) => toast.error(error.message),
  })

  if (query.isLoading) return <LoadingState rows={6} />
  if (query.error) return <ErrorState error={query.error} retry={() => void query.refetch()} />

  const servers = query.data?.servers ?? []
  const builtins = servers.filter((server) => server.descriptor.builtIn)
  const custom = servers.filter((server) => !server.descriptor.builtIn)

  const toggleBuiltin = async (id: string, enabled: boolean) => {
    const states = Object.fromEntries(builtins.map((server) => [server.descriptor.id, server.descriptor.id === id ? enabled : server.enabled]))
    try {
      await api("/mcp-servers/builtins", { method: "PUT", ...jsonBody({ servers: states }) })
      await refresh()
    } catch (error) {
      toast.error(error instanceof Error ? error.message : String(error))
    }
  }
  const saveCustomServers = (items: McpServerAggregate[], replacement?: { id: string; config: Record<string, unknown> }) => {
    const records: Record<string, Record<string, unknown>> = Object.fromEntries(items.map((server) => [server.descriptor.id, mcpRequest(server)]))
    if (replacement) records[replacement.id] = replacement.config
    return api("/mcp-servers", { method: "PUT", ...jsonBody({ servers: records, clear_secrets: {} }) })
  }
  const remove = async (id: string) => {
    const remaining = custom.filter((server) => server.descriptor.id !== id)
    try {
      await saveCustomServers(remaining)
      await refresh()
      toast.success("MCP server deleted")
    } catch (error) {
      toast.error(error instanceof Error ? error.message : String(error))
    }
  }

  return (
    <>
      <SettingsHeader title="MCP Servers" description="Built-in and custom tool servers reconciled by the PL generation runtime." status={<Badge variant="outline">{servers.filter((server) => server.enabled).length} enabled</Badge>} />
      <SettingsBody>
        <section className="overflow-hidden rounded-lg border">
          <div className="flex items-center justify-between gap-3 border-b px-4 py-3">
            <div><h2 className="text-sm font-semibold">Built-in services</h2><p className="text-xs text-muted-foreground">Identity and endpoint are owned by PL; Mai controls availability.</p></div>
            <Button variant="outline" disabled={recheck.isPending} onClick={() => recheck.mutate()}>{recheck.isPending ? <Spinner data-icon="inline-start" /> : <RefreshCw data-icon="inline-start" />} Recheck</Button>
          </div>
          {builtins.length === 0 ? <EmptyState title="No built-in services" description="PL did not publish any built-in MCP descriptors." /> : <Table><TableHeader><TableRow><TableHead>Service</TableHead><TableHead>Availability</TableHead><TableHead>Runtime</TableHead><TableHead className="text-right">Enabled</TableHead></TableRow></TableHeader><TableBody>
            {builtins.map((server) => <TableRow key={server.descriptor.id}><TableCell><ServerIdentity server={server} /></TableCell><TableCell><StatusBadge status={server.availability} /></TableCell><TableCell className="text-muted-foreground">{server.tool_count} tools · {server.ready_agents}/{server.total_agents} agents</TableCell><TableCell className="text-right"><Switch aria-label={`Enable ${server.descriptor.id}`} checked={server.enabled} onCheckedChange={(checked: boolean) => void toggleBuiltin(server.descriptor.id, checked)} /></TableCell></TableRow>)}
          </TableBody></Table>}
        </section>

        <section className="mt-6 overflow-hidden rounded-lg border">
          <div className="flex items-center justify-between gap-3 border-b px-4 py-3">
            <div><h2 className="text-sm font-semibold">Custom servers</h2><p className="text-xs text-muted-foreground">Stdio runs in the agent container; Streamable HTTP connects from Mai.</p></div>
            <Button onClick={() => setEditing("new")}><Plus data-icon="inline-start" /> Add server</Button>
          </div>
          {custom.length === 0 ? <EmptyState title="No custom MCP servers" description="Add an agent, project, or system-scoped server." /> : <Table><TableHeader><TableRow><TableHead>Server</TableHead><TableHead>Scope</TableHead><TableHead>Availability</TableHead><TableHead className="w-24 text-right">Actions</TableHead></TableRow></TableHeader><TableBody>
            {custom.map((server) => <TableRow key={server.descriptor.id}><TableCell><ServerIdentity server={server} /></TableCell><TableCell><Badge variant="outline">{server.config?.scope}</Badge></TableCell><TableCell><StatusBadge status={server.availability} /></TableCell><TableCell><div className="flex justify-end gap-1"><Button size="icon" variant="ghost" aria-label={`Edit ${server.descriptor.id}`} onClick={() => setEditing(server)}><Pencil data-icon="inline-start" /></Button><AlertDialog><AlertDialogTrigger asChild><Button size="icon" variant="ghost" aria-label={`Delete ${server.descriptor.id}`}><Trash2 data-icon="inline-start" /></Button></AlertDialogTrigger><AlertDialogContent><AlertDialogHeader><AlertDialogTitle>Delete {server.descriptor.id}?</AlertDialogTitle><AlertDialogDescription>This removes the server from future agent MCP generations. Active turns keep their current lease.</AlertDialogDescription></AlertDialogHeader><AlertDialogFooter><AlertDialogCancel>Cancel</AlertDialogCancel><AlertDialogAction variant="destructive" onClick={() => void remove(server.descriptor.id)}>Delete server</AlertDialogAction></AlertDialogFooter></AlertDialogContent></AlertDialog></div></TableCell></TableRow>)}
          </TableBody></Table>}
        </section>
      </SettingsBody>
      {editing && <McpEditor server={editing === "new" ? null : editing} onClose={() => setEditing(null)} onSave={async (id, config) => {
        const remaining = custom.filter((server) => !editing || editing === "new" || server.descriptor.id !== editing.descriptor.id)
        await saveCustomServers(remaining, { id, config })
        await refresh()
        setEditing(null)
        toast.success("MCP server saved")
      }} />}
    </>
  )
}

function ServerIdentity({ server }: { server: McpServerAggregate }) {
  return <div className="flex min-w-0 items-center gap-3"><Avatar className="size-9 rounded-lg"><AvatarFallback className="rounded-lg"><ServerCog className="size-4" /></AvatarFallback></Avatar><div className="min-w-0"><strong className="block truncate text-sm">{server.descriptor.id}</strong><p className="max-w-80 truncate text-xs text-muted-foreground">{server.descriptor.endpoint || server.descriptor.transport}</p></div></div>
}

function McpEditor({ server, onClose, onSave }: { server: McpServerAggregate | null; onClose(): void; onSave(id: string, config: Record<string, unknown>): Promise<void> }) {
  const [form, setForm] = useState<McpForm>(() => formFromServer(server))
  const [error, setError] = useState("")
  const [saving, setSaving] = useState(false)
  const save = async () => {
    setError("")
    if (!form.id || (form.transport === "stdio" ? !form.command : !form.url)) return setError("Server ID and transport endpoint are required.")
    const env = parseMap(form.env)
    const headers = parseMap(form.headers)
    if (env.error || headers.error) return setError(env.error || headers.error || "Invalid JSON")
    setSaving(true)
    try {
      await onSave(form.id, { scope: form.scope, transport: form.transport, command: form.command || null, args: form.args.split("\n").map((item) => item.trim()).filter(Boolean), env: env.value, cwd: form.cwd || null, url: form.url || null, headers: headers.value, bearer_token: form.bearerToken || null, bearer_token_env: form.bearerTokenEnv || null, enabled: form.enabled, required: form.required, startup_timeout_secs: null, tool_timeout_secs: null, enabled_tools: null, disabled_tools: [] })
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause))
    } finally {
      setSaving(false)
    }
  }

  return <Dialog open onOpenChange={(open: boolean) => { if (!open) onClose() }}><DialogContent className="max-h-[90vh] overflow-auto sm:max-w-2xl"><DialogHeader><DialogTitle>{server ? `Edit ${server.descriptor.id}` : "Add MCP server"}</DialogTitle><DialogDescription>Secrets are write-only; blank values retain the saved value on edits.</DialogDescription></DialogHeader><FieldGroup>
    <div className="grid gap-4 sm:grid-cols-2">
      <FormField label="Server ID"><Input disabled={Boolean(server)} value={form.id} onChange={(event) => setForm({ ...form, id: event.target.value })} /></FormField>
      <FormField label="Scope"><Select value={form.scope} onValueChange={(scope: string) => setForm({ ...form, scope })}><SelectTrigger className="w-full"><SelectValue /></SelectTrigger><SelectContent><SelectGroup><SelectItem value="agent">Agent</SelectItem><SelectItem value="project">Project</SelectItem><SelectItem value="system">System</SelectItem></SelectGroup></SelectContent></Select></FormField>
      <FormField label="Transport"><Select value={form.transport} onValueChange={(transport: string) => setForm({ ...form, transport })}><SelectTrigger className="w-full"><SelectValue /></SelectTrigger><SelectContent><SelectGroup><SelectItem value="stdio">Stdio</SelectItem><SelectItem value="streamable_http">Streamable HTTP</SelectItem></SelectGroup></SelectContent></Select></FormField>
      {form.transport === "stdio" ? <FormField label="Command"><Input value={form.command} onChange={(event) => setForm({ ...form, command: event.target.value })} /></FormField> : <FormField label="URL"><Input value={form.url} onChange={(event) => setForm({ ...form, url: event.target.value })} /></FormField>}
      <FormField label="Arguments" hint="One argument per line"><Textarea value={form.args} onChange={(event) => setForm({ ...form, args: event.target.value })} /></FormField>
      <FormField label="Working directory"><Input value={form.cwd} onChange={(event) => setForm({ ...form, cwd: event.target.value })} /></FormField>
      <FormField label="Environment (JSON)"><Textarea className="font-mono text-xs" value={form.env} onChange={(event) => setForm({ ...form, env: event.target.value })} /></FormField>
      <FormField label="Headers (JSON)"><Textarea className="font-mono text-xs" value={form.headers} onChange={(event) => setForm({ ...form, headers: event.target.value })} /></FormField>
      <FormField label="Bearer token"><Input type="password" value={form.bearerToken} onChange={(event) => setForm({ ...form, bearerToken: event.target.value })} /></FormField>
      <FormField label="Bearer token env"><Input value={form.bearerTokenEnv} onChange={(event) => setForm({ ...form, bearerTokenEnv: event.target.value })} /></FormField>
    </div>
    <div className="grid gap-3 sm:grid-cols-2">
      <Field orientation="horizontal" className="rounded-lg border p-3"><FieldContent><FieldTitle>Enabled</FieldTitle><FieldDescription>Expose this server to eligible agents.</FieldDescription></FieldContent><Switch aria-label="Enable MCP server" checked={form.enabled} onCheckedChange={(enabled: boolean) => setForm({ ...form, enabled })} /></Field>
      <Field orientation="horizontal" className="rounded-lg border p-3"><FieldContent><FieldTitle>Required</FieldTitle><FieldDescription>Block provisioning when connection fails.</FieldDescription></FieldContent><Switch aria-label="Require MCP server" checked={form.required} onCheckedChange={(required: boolean) => setForm({ ...form, required })} /></Field>
    </div>
    {error && <Alert variant="destructive"><AlertDescription>{error}</AlertDescription></Alert>}
  </FieldGroup><DialogFooter><Button variant="outline" onClick={onClose}>Cancel</Button><Button disabled={saving} onClick={() => void save()}>{saving && <Spinner data-icon="inline-start" />} Save server</Button></DialogFooter></DialogContent></Dialog>
}

function formFromServer(server: McpServerAggregate | null): McpForm {
  const config = server?.config
  return { id: server?.descriptor.id || "", scope: config?.scope || "agent", transport: server?.descriptor.transport || "stdio", command: config?.command || "", args: config?.args.join("\n") || "", cwd: config?.cwd || "", url: config?.url || "", env: JSON.stringify(Object.fromEntries((config?.env_keys ?? []).map((key) => [key, ""])), null, 2), headers: JSON.stringify(Object.fromEntries((config?.header_names ?? []).map((key) => [key, ""])), null, 2), bearerToken: "", bearerTokenEnv: config?.bearer_token_env || "", enabled: config?.enabled ?? true, required: config?.required ?? false }
}

function mcpRequest(server: McpServerAggregate) {
  const config = server.config
  return { scope: config?.scope || "agent", transport: server.descriptor.transport, command: config?.command || null, args: config?.args || [], env: Object.fromEntries((config?.env_keys ?? []).map((key) => [key, ""])), cwd: config?.cwd || null, url: config?.url || null, headers: Object.fromEntries((config?.header_names ?? []).map((key) => [key, ""])), bearer_token: null, bearer_token_env: config?.bearer_token_env || null, enabled: config?.enabled ?? server.enabled, required: config?.required ?? false, startup_timeout_secs: config?.startup_timeout_secs || null, tool_timeout_secs: config?.tool_timeout_secs || null, enabled_tools: config?.enabled_tools || null, disabled_tools: config?.disabled_tools || [] }
}

function parseMap(value: string): { value?: Record<string, string>; error?: string } {
  try {
    const parsed = JSON.parse(value || "{}") as unknown
    if (!parsed || Array.isArray(parsed) || typeof parsed !== "object") return { error: "Expected a JSON object." }
    return { value: Object.fromEntries(Object.entries(parsed).map(([key, item]) => [key, String(item)])) }
  } catch (error) {
    return { error: error instanceof Error ? error.message : "Invalid JSON" }
  }
}
