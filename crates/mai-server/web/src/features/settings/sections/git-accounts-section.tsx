import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query"
import { CheckCircle2, KeyRound, Plus, RefreshCw, Trash2 } from "lucide-react"
import { useState } from "react"
import { toast } from "sonner"

import { api, jsonBody } from "@/api/client"
import type { GitAccountSummary, GitAccountsResponse } from "@/api/product-types"
import { queryKeys } from "@/api/queries"
import { EmptyState, ErrorState, LoadingState } from "@/components/page-state"
import { StatusBadge } from "@/components/status"
import { Alert, AlertDescription } from "@/components/ui/alert"
import { AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent, AlertDialogDescription, AlertDialogFooter, AlertDialogHeader, AlertDialogTitle, AlertDialogTrigger } from "@/components/ui/alert-dialog"
import { Avatar, AvatarFallback } from "@/components/ui/avatar"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { FieldGroup } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { FormField, SettingsBody, SettingsHeader } from "@/features/settings/settings-section"
import { cn } from "@/lib/utils"

interface AccountForm { id: string; label: string; token: string }

export function GitAccountsSection() {
  const queryClient = useQueryClient()
  const query = useQuery({ queryKey: queryKeys.gitAccounts, queryFn: () => api<GitAccountsResponse>("/git/accounts") })
  const [form, setForm] = useState<AccountForm>({ id: "", label: "", token: "" })
  const refresh = () => queryClient.invalidateQueries({ queryKey: queryKeys.gitAccounts })
  const save = useMutation({
    mutationFn: () => api(form.id ? `/git/accounts/${encodeURIComponent(form.id)}` : "/git/accounts", { method: form.id ? "PUT" : "POST", ...jsonBody({ id: form.id || null, provider: "github", label: form.label, token: form.token || null, is_default: false }) }),
    onSuccess: async () => { toast.success("Git account saved"); setForm({ id: "", label: "", token: "" }); await refresh() },
    onError: (error) => toast.error(error.message),
  })
  if (query.isLoading) return <LoadingState rows={6} />
  if (query.error) return <ErrorState error={query.error} retry={() => void query.refetch()} />
  const accounts = query.data?.accounts ?? []

  const select = (account: GitAccountSummary) => setForm({ id: account.id, label: account.label, token: "" })
  const verify = async (id: string) => {
    try { await api(`/git/accounts/${encodeURIComponent(id)}/verify`, { method: "POST" }); toast.success("Verification started"); await refresh() } catch (error) { toast.error(error instanceof Error ? error.message : String(error)) }
  }
  const makeDefault = async (id: string) => {
    try { await api("/git/accounts/default", { method: "PUT", ...jsonBody({ account_id: id }) }); await refresh() } catch (error) { toast.error(error instanceof Error ? error.message : String(error)) }
  }
  const remove = async (id: string) => {
    try { await api(`/git/accounts/${encodeURIComponent(id)}`, { method: "DELETE" }); if (form.id === id) setForm({ id: "", label: "", token: "" }); await refresh() } catch (error) { toast.error(error instanceof Error ? error.message : String(error)) }
  }

  return <>
    <SettingsHeader title="Git Accounts" description="Manage server-side GitHub credentials used for repositories and MCP tools." status={<Badge variant="outline">{accounts.length} connected</Badge>} />
    <SettingsBody>
      <div className="grid gap-5 lg:grid-cols-[1fr_1.15fr]">
        <div className="divide-y overflow-hidden rounded-lg border">
          {accounts.map((account) => <button key={account.id} onClick={() => select(account)} className={cn("flex w-full items-center gap-3 px-4 py-3 text-left hover:bg-muted", form.id === account.id && "bg-muted")}>
            <Avatar className="size-9 rounded-lg"><AvatarFallback className="rounded-lg"><KeyRound className="size-4" /></AvatarFallback></Avatar>
            <span className="min-w-0 flex-1"><span className="flex items-center gap-2"><strong className="truncate text-sm">{account.label}</strong>{account.is_default && <Badge variant="secondary">Default</Badge>}</span><small className="block truncate text-xs text-muted-foreground">{account.login || "Unverified"} · {account.token_kind}</small></span>
            <StatusBadge status={account.status} />
          </button>)}
          {!accounts.length && <EmptyState title="No Git accounts" description="Add a GitHub personal access token to connect repositories." />}
        </div>
        <div className="rounded-lg border p-5">
          <div className="mb-5 flex items-center justify-between"><div><h2 className="text-sm font-semibold">{form.id ? "Edit account" : "Add account"}</h2><p className="mt-1 text-xs text-muted-foreground">Stored secrets are write-only. A blank token keeps the current value.</p></div>{form.id && <Button size="sm" variant="ghost" onClick={() => setForm({ id: "", label: "", token: "" })}><Plus data-icon="inline-start" /> New</Button>}</div>
          <FieldGroup>
            <FormField label="Account label"><Input value={form.label} onChange={(event) => setForm({ ...form, label: event.target.value })} placeholder="Personal GitHub" /></FormField>
            <FormField label="Token" hint={form.id ? "Leave blank to preserve the saved token." : "Classic and fine-grained PATs are supported."}><Input type="password" value={form.token} onChange={(event) => setForm({ ...form, token: event.target.value })} placeholder={form.id ? "Saved token retained" : "github_pat_…"} /></FormField>
            {form.id && query.data?.accounts.find((account) => account.id === form.id)?.last_error && <Alert variant="destructive"><AlertDescription>{query.data.accounts.find((account) => account.id === form.id)?.last_error}</AlertDescription></Alert>}
            <div className="flex flex-wrap gap-2 pt-2">
              <Button disabled={!form.label || (!form.id && !form.token) || save.isPending} onClick={() => save.mutate()}><CheckCircle2 data-icon="inline-start" /> Save account</Button>
              {form.id && <Button variant="outline" onClick={() => void verify(form.id)}><RefreshCw data-icon="inline-start" /> Test</Button>}
              {form.id && !query.data?.accounts.find((account) => account.id === form.id)?.is_default && <Button variant="outline" onClick={() => void makeDefault(form.id)}>Set default</Button>}
              {form.id && <AlertDialog><AlertDialogTrigger asChild><Button variant="ghost" className="ml-auto"><Trash2 data-icon="inline-start" /> Delete</Button></AlertDialogTrigger><AlertDialogContent><AlertDialogHeader><AlertDialogTitle>Delete this Git account?</AlertDialogTitle><AlertDialogDescription>Projects using this credential may no longer be able to fetch repositories.</AlertDialogDescription></AlertDialogHeader><AlertDialogFooter><AlertDialogCancel>Cancel</AlertDialogCancel><AlertDialogAction variant="destructive" onClick={() => void remove(form.id)}>Delete account</AlertDialogAction></AlertDialogFooter></AlertDialogContent></AlertDialog>}
            </div>
          </FieldGroup>
        </div>
      </div>
    </SettingsBody>
  </>
}
