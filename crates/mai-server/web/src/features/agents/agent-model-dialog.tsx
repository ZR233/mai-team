import { useQuery } from "@tanstack/react-query"
import { Check, Pencil } from "lucide-react"
import { useEffect, useMemo, useState } from "react"
import { toast } from "sonner"

import { api, jsonBody } from "@/api/client"
import type { AgentDetail, ProviderModel } from "@/api/product-types"
import { providersQuery } from "@/api/queries"
import { Alert, AlertDescription } from "@/components/ui/alert"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger } from "@/components/ui/dialog"
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field"
import { Select, SelectContent, SelectGroup, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Spinner } from "@/components/ui/spinner"

export function AgentModelDialog({ agent, onSaved }: { agent: AgentDetail; onSaved(): Promise<unknown> }) {
  const providers = useQuery(providersQuery())
  const [open, setOpen] = useState(false)
  const [providerId, setProviderId] = useState(agent.provider_id)
  const [model, setModel] = useState(agent.model)
  const [reasoning, setReasoning] = useState(agent.reasoning_effort || "default")
  const [saving, setSaving] = useState(false)

  useEffect(() => {
    if (!open) return
    setProviderId(agent.provider_id)
    setModel(agent.model)
    setReasoning(agent.reasoning_effort || "default")
  }, [agent.model, agent.provider_id, agent.reasoning_effort, open])

  const provider = providers.data?.providers.find((candidate) => candidate.id === providerId)
  const selectedModel = provider?.models.find((candidate) => candidate.id === model)
  const reasoningOptions = useMemo(() => modelReasoningOptions(selectedModel), [selectedModel])

  const selectProvider = (nextProviderId: string) => {
    const next = providers.data?.providers.find((candidate) => candidate.id === nextProviderId)
    const nextModel = next?.models.find((candidate) => candidate.id === next.default_model) ?? next?.models[0]
    setProviderId(nextProviderId)
    setModel(nextModel?.id || "")
    setReasoning(defaultReasoning(nextModel))
  }

  const selectModel = (nextModelId: string) => {
    const next = provider?.models.find((candidate) => candidate.id === nextModelId)
    setModel(nextModelId)
    setReasoning(defaultReasoning(next))
  }

  const save = async () => {
    if (!providerId || !model) return
    setSaving(true)
    try {
      await api(`/agents/${agent.id}`, {
        method: "PATCH",
        ...jsonBody({ provider_id: providerId, model, reasoning_effort: reasoning === "default" ? null : reasoning }),
      })
      await onSaved()
      setOpen(false)
      toast.success("Agent model updated")
    } catch (error) {
      toast.error(error instanceof Error ? error.message : String(error))
    } finally {
      setSaving(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild><Button variant="ghost" size="icon-sm" aria-label={`Edit model for ${agent.name}`}><Pencil data-icon="inline-start" /></Button></DialogTrigger>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader><DialogTitle>Agent model</DialogTitle><DialogDescription>Choose from the effective provider catalog returned by the server.</DialogDescription></DialogHeader>
        {providers.isLoading && <div className="flex items-center gap-2 text-sm text-muted-foreground"><Spinner /> Loading providers…</div>}
        {providers.error && <Alert variant="destructive"><AlertDescription>{providers.error.message}</AlertDescription></Alert>}
        {providers.data && <FieldGroup>
          <Field><FieldLabel>Provider</FieldLabel><Select value={providerId} onValueChange={selectProvider}><SelectTrigger className="w-full"><SelectValue /></SelectTrigger><SelectContent><SelectGroup>{providers.data.providers.filter((item) => item.enabled).map((item) => <SelectItem key={item.id} value={item.id}>{item.name}</SelectItem>)}</SelectGroup></SelectContent></Select></Field>
          <Field><FieldLabel>Model</FieldLabel><Select value={model} onValueChange={selectModel}><SelectTrigger className="w-full"><SelectValue /></SelectTrigger><SelectContent><SelectGroup>{(provider?.models ?? []).map((item) => <SelectItem key={item.id} value={item.id}>{item.display_name || item.name || item.id}</SelectItem>)}</SelectGroup></SelectContent></Select></Field>
          <Field><FieldLabel>Reasoning</FieldLabel><Select value={reasoning} onValueChange={setReasoning}><SelectTrigger className="w-full"><SelectValue /></SelectTrigger><SelectContent><SelectGroup><SelectItem value="default">Provider default</SelectItem>{reasoningOptions.map((item) => <SelectItem key={item.id} value={item.id}>{item.label}</SelectItem>)}</SelectGroup></SelectContent></Select></Field>
        </FieldGroup>}
        <DialogFooter><Button variant="outline" onClick={() => setOpen(false)}>Cancel</Button><Button disabled={saving || !providerId || !model} onClick={() => void save()}>{saving ? <Spinner data-icon="inline-start" /> : <Check data-icon="inline-start" />}{saving ? "Saving…" : "Apply"}</Button></DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

function modelReasoningOptions(model?: ProviderModel) {
  const variants = model?.reasoning?.variants ?? []
  if (variants.length) return variants.map((item) => ({ id: item.id, label: item.label || item.id }))
  return (model?.reasoning?.candidates ?? []).map((item) => ({ id: item, label: item }))
}

function defaultReasoning(model?: ProviderModel) {
  return model?.reasoning?.default_variant || model?.reasoning?.default || "default"
}
