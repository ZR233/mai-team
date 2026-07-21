import { useMutation, useQuery } from "@tanstack/react-query"
import { Check, Github, KeyRound, RefreshCw } from "lucide-react"
import { useEffect, useMemo, useState } from "react"
import { Link } from "react-router-dom"
import { toast } from "sonner"

import { api, jsonBody } from "@/api/client"
import type {
  GitAccountsResponse,
  GithubAppSettings,
  GithubInstallationSummary,
  GithubRepositorySummary,
  ProjectSummary,
  RelayStatus,
  RepositoryPackageSummary,
  RuntimeDefaults,
} from "@/api/product-types"
import { queryKeys } from "@/api/queries"
import { StatusBadge } from "@/components/status"
import { Alert, AlertDescription } from "@/components/ui/alert"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Empty, EmptyDescription, EmptyHeader, EmptyTitle } from "@/components/ui/empty"
import { Field, FieldContent, FieldDescription, FieldGroup, FieldTitle } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectGroup, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Spinner } from "@/components/ui/spinner"
import { Switch } from "@/components/ui/switch"
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs"
import { Textarea } from "@/components/ui/textarea"

type SourceMode = "git_account" | "github_app"

export function CreateProjectDialog({ open, onOpenChange, onCreated }: {
  open: boolean
  onOpenChange(open: boolean): void
  onCreated(project: ProjectSummary): void
}) {
  const accounts = useQuery({ queryKey: queryKeys.gitAccounts, queryFn: () => api<GitAccountsResponse>("/git/accounts"), enabled: open })
  const relay = useQuery({ queryKey: queryKeys.relay, queryFn: () => api<RelayStatus>("/relay/status"), enabled: open })
  const app = useQuery({ queryKey: queryKeys.githubApp, queryFn: () => api<GithubAppSettings>("/settings/github-app"), enabled: open })
  const installations = useQuery({ queryKey: [...queryKeys.githubApp, "installations"], queryFn: () => api<{ installations: GithubInstallationSummary[] }>("/github/installations"), enabled: open })
  const defaults = useQuery({ queryKey: ["runtime-defaults"], queryFn: () => api<RuntimeDefaults>("/runtime/defaults"), enabled: open, staleTime: Number.POSITIVE_INFINITY })
  const [mode, setMode] = useState<SourceMode>("git_account")
  const [sourceId, setSourceId] = useState("")
  const [repositoryName, setRepositoryName] = useState("")
  const [query, setQuery] = useState("")
  const [name, setName] = useState("")
  const [branch, setBranch] = useState("")
  const [image, setImage] = useState("")
  const [autoReview, setAutoReview] = useState(false)
  const [reviewerPrompt, setReviewerPrompt] = useState("")

  const sources = useMemo(
    () => mode === "git_account" ? accounts.data?.accounts ?? [] : installations.data?.installations ?? [],
    [accounts.data?.accounts, installations.data?.installations, mode],
  )
  useEffect(() => {
    if (!sourceId && sources[0]) setSourceId(String(sources[0].id))
  }, [sourceId, sources])

  const repositories = useQuery({
    queryKey: ["project-create-repositories", mode, sourceId],
    queryFn: () => api<{ repositories: GithubRepositorySummary[] }>(mode === "git_account"
      ? `/git/accounts/${encodeURIComponent(sourceId)}/repositories`
      : `/github/installations/${encodeURIComponent(sourceId)}/repositories`),
    enabled: open && Boolean(sourceId),
  })
  const selected = repositories.data?.repositories.find((repository) => repository.full_name === repositoryName)
  const packages = useQuery({
    queryKey: ["project-create-packages", mode, sourceId, repositoryName],
    queryFn: () => {
      const [owner, repo] = repositoryName.split("/", 2)
      const root = mode === "git_account" ? `/git/accounts/${encodeURIComponent(sourceId)}` : `/github/installations/${encodeURIComponent(sourceId)}`
      return api<{ packages: RepositoryPackageSummary[]; warning?: string | null }>(`${root}/repositories/${encodeURIComponent(owner)}/${encodeURIComponent(repo)}/packages`)
    },
    enabled: open && Boolean(sourceId && repositoryName.includes("/")),
  })
  const visibleRepositories = useMemo(() => {
    const needle = query.trim().toLowerCase()
    return (repositories.data?.repositories ?? []).filter((repository) => !needle || repository.full_name.toLowerCase().includes(needle)).slice(0, 8)
  }, [query, repositories.data])

  const create = useMutation({
    mutationFn: () => {
      if (!selected) throw new Error("Select a repository first.")
      const [owner, repo] = selected.full_name.split("/", 2)
      return api<{ project: ProjectSummary }>("/projects", {
        method: "POST",
        ...jsonBody({
          name: name.trim() || selected.name,
          git_account_id: mode === "git_account" ? sourceId : null,
          installation_id: mode === "github_app" ? Number(sourceId) : 0,
          repository_id: selected.id,
          repository_full_name: selected.full_name,
          owner,
          repo,
          branch: branch.trim() || selected.default_branch || null,
          docker_image: image.trim() || null,
          auto_review_enabled: autoReview,
          reviewer_extra_prompt: reviewerPrompt.trim() || null,
        }),
      })
    },
    onSuccess: ({ project }) => { toast.success("Project created"); onCreated(project) },
    onError: (error) => toast.error(error.message),
  })

  const chooseMode = (next: string) => {
    setMode(next as SourceMode)
    setSourceId("")
    setRepositoryName("")
    setQuery("")
    setBranch("")
  }
  const chooseRepository = (repository: GithubRepositorySummary) => {
    setRepositoryName(repository.full_name)
    setBranch(repository.default_branch || "main")
    if (!name.trim()) setName(repository.name)
  }
  const loading = accounts.isLoading || relay.isLoading || app.isLoading || installations.isLoading || defaults.isLoading
  const selectedInstallation = installations.data?.installations.find((installation) => String(installation.id) === sourceId)
  const appReady = Boolean(relay.data?.connected && (app.data?.app_slug || app.data?.install_url))

  return <Dialog open={open} onOpenChange={onOpenChange}><DialogContent className="max-h-[92vh] overflow-auto sm:max-w-3xl"><DialogHeader><DialogTitle>New project</DialogTitle><DialogDescription>Select a verified GitHub source, repository, and isolated runtime.</DialogDescription></DialogHeader>
    <Tabs value={mode} onValueChange={chooseMode}><TabsList className="grid w-full grid-cols-2"><TabsTrigger value="git_account"><KeyRound data-icon="inline-start" /> Git account</TabsTrigger><TabsTrigger value="github_app"><Github data-icon="inline-start" /> GitHub App</TabsTrigger></TabsList></Tabs>
    {loading ? <div className="flex items-center justify-center gap-2 py-10 text-sm text-muted-foreground"><Spinner /> Loading repository sources…</div> : <div className="grid gap-6 md:grid-cols-[1fr_1.2fr]">
      <FieldGroup>
        {mode === "git_account" && !sources.length ? <SourceEmpty title="No Git accounts" href="/settings/git-accounts" /> : null}
        {mode === "github_app" && (!appReady || !sources.length) ? <SourceEmpty title={appReady ? "No installations" : "GitHub App unavailable"} href="/settings/github-app" /> : null}
        {sources.length > 0 && <ProjectField label={mode === "git_account" ? "Git account" : "Installation"}><Select value={sourceId} onValueChange={(value: string) => { setSourceId(value); setRepositoryName(""); setQuery("") }}><SelectTrigger className="w-full"><SelectValue /></SelectTrigger><SelectContent><SelectGroup>{sources.map((source) => <SelectItem key={source.id} value={String(source.id)}>{mode === "git_account" ? "label" in source ? source.label : source.id : "account_login" in source ? source.account_login : source.id}</SelectItem>)}</SelectGroup></SelectContent></Select></ProjectField>}
        {selectedInstallation && !selectedInstallation.events.includes("pull_request") && <Alert><AlertDescription>This installation does not subscribe to pull_request events; automatic review triggers will be unavailable.</AlertDescription></Alert>}
        <ProjectField label="Project name"><Input value={name} onChange={(event) => setName(event.target.value)} placeholder={selected?.name || "Project name"} /></ProjectField>
        <ProjectField label="Branch"><Input value={branch} onChange={(event) => setBranch(event.target.value)} placeholder={selected?.default_branch || "Default branch"} /></ProjectField>
        <ProjectField label="Docker image"><Input value={image} onChange={(event) => setImage(event.target.value)} placeholder={defaults.data?.default_docker_image || "Server default"} /></ProjectField>
        {(packages.data?.packages.length ?? 0) > 0 && <ProjectField label="Repository image"><Select value={image} onValueChange={setImage}><SelectTrigger className="w-full"><SelectValue placeholder="Choose an image" /></SelectTrigger><SelectContent><SelectGroup>{packages.data?.packages.map((item) => <SelectItem key={item.image} value={item.image}>{item.image}</SelectItem>)}</SelectGroup></SelectContent></Select></ProjectField>}
        {packages.data?.warning && <Alert><AlertDescription>{packages.data.warning}</AlertDescription></Alert>}
      </FieldGroup>
      <FieldGroup>
        <ProjectField label="Find repository"><div className="flex gap-2"><Input value={query} onChange={(event) => setQuery(event.target.value)} placeholder="owner/repository" /><Button variant="outline" size="icon" onClick={() => void repositories.refetch()} aria-label="Refresh repositories"><RefreshCw data-icon="inline-start" /></Button></div></ProjectField>
        <div className="max-h-64 divide-y overflow-auto rounded-lg border">{visibleRepositories.map((repository) => <button type="button" key={repository.id} onClick={() => chooseRepository(repository)} className="flex w-full items-center gap-3 px-3 py-2.5 text-left hover:bg-muted"><span className="min-w-0 flex-1"><strong className="block truncate text-sm">{repository.full_name}</strong><small className="text-xs text-muted-foreground">{repository.default_branch || "main"}{repository.private ? " · private" : ""}</small></span>{repository.full_name === repositoryName && <Check className="size-4" />}</button>)}{repositories.isLoading && <div className="flex items-center justify-center gap-2 p-5 text-xs text-muted-foreground"><Spinner /> Loading repositories…</div>}{!repositories.isLoading && !visibleRepositories.length && <div className="p-5 text-center text-xs text-muted-foreground">No repositories found.</div>}</div>
        <Field orientation="horizontal" className="rounded-lg border p-3"><FieldContent><FieldTitle>Automatic review</FieldTitle><FieldDescription>Watch eligible pull requests.</FieldDescription></FieldContent><Switch aria-label="Enable automatic review" checked={autoReview} onCheckedChange={setAutoReview} /></Field>
        <ProjectField label="Reviewer instructions"><Textarea value={reviewerPrompt} onChange={(event) => setReviewerPrompt(event.target.value)} placeholder="Optional project-specific review focus" /></ProjectField>
        <Alert><AlertDescription><StatusBadge status={selected ? "ready" : "incomplete"} /> <span className="ml-2">{selected ? `${selected.full_name} is ready to provision.` : "Choose a repository to continue."}</span></AlertDescription></Alert>
      </FieldGroup>
    </div>}
    <DialogFooter><Button variant="outline" onClick={() => onOpenChange(false)}>Cancel</Button><Button disabled={!selected || create.isPending || (mode === "github_app" && !selectedInstallation?.events.includes("pull_request"))} onClick={() => create.mutate()}>{create.isPending && <Spinner data-icon="inline-start" />} {create.isPending ? "Creating…" : "Create project"}</Button></DialogFooter>
  </DialogContent></Dialog>
}

function SourceEmpty({ title, href }: { title: string; href: string }) {
  return <Empty className="border border-dashed py-5"><EmptyHeader><EmptyTitle>{title}</EmptyTitle><EmptyDescription>Configure a repository source before creating a project.</EmptyDescription></EmptyHeader><Button asChild variant="outline" size="sm"><Link to={href}>Open settings</Link></Button></Empty>
}

function ProjectField({ label, children }: { label: string; children: React.ReactNode }) {
  return <Field><FieldTitle>{label}</FieldTitle>{children}</Field>
}
