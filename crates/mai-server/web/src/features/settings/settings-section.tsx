import { RotateCcw, Save } from "lucide-react"

import { Button } from "@/components/ui/button"
import { Field, FieldDescription, FieldLabel } from "@/components/ui/field"
import { Spinner } from "@/components/ui/spinner"

export function SettingsHeader({ title, description, status }: { title: string; description: string; status?: React.ReactNode }) {
  return (
    <header className="flex min-h-20 shrink-0 items-center justify-between gap-4 border-b px-5 md:px-8">
      <div><p className="text-base font-semibold">{title}</p><p className="mt-1 text-xs text-muted-foreground">{description}</p></div>
      {status}
    </header>
  )
}

export function SettingsBody({ children }: { children: React.ReactNode }) {
  return <div className="min-h-0 flex-1 overflow-auto"><div className="mx-auto max-w-6xl p-5 pb-24 md:p-8 md:pb-24">{children}</div></div>
}

export function StickyActions({ dirty, saving, onReset, onSave, error }: {
  dirty: boolean
  saving: boolean
  onReset(): void
  onSave(): void
  error?: string | null
}) {
  return (
    <footer className="absolute inset-x-0 bottom-0 z-20 flex min-h-16 items-center justify-end gap-2 border-t bg-background/95 px-5 backdrop-blur md:px-8">
      {error && <p className="mr-auto text-xs text-destructive">{error}</p>}
      <Button variant="outline" disabled={!dirty || saving} onClick={onReset}><RotateCcw data-icon="inline-start" /> Reset</Button>
      <Button disabled={!dirty || saving} onClick={onSave}>{saving ? <Spinner data-icon="inline-start" /> : <Save data-icon="inline-start" />} {saving ? "Saving…" : "Save changes"}</Button>
    </footer>
  )
}

export function FormField({ label, hint, htmlFor, children }: { label: string; hint?: string; htmlFor?: string; children: React.ReactNode }) {
  return <Field><FieldLabel htmlFor={htmlFor}>{label}</FieldLabel>{children}{hint && <FieldDescription>{hint}</FieldDescription>}</Field>
}
