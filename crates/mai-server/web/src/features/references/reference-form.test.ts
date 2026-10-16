import { describe, expect, it } from "vitest"

import type { ProjectReferenceBinding } from "@/api/reference-types"

import {
  aliasContainerPath,
  defaultReferenceLimits,
  draftDigest,
  newBindingId,
  sameBindings,
  validateBindings,
  validateSourceDraft,
} from "@/features/references/reference-form"

function binding(id: string, alias: string, overrides: Partial<ProjectReferenceBinding> = {}): ProjectReferenceBinding {
  return { id, source_id: "source-1", alias, description: "", enabled: true, ...overrides }
}

describe("reference binding validation", () => {
  it("accepts a normalized binding and derives the container target from the alias", () => {
    const result = validateBindings([binding("b1", "linux")])

    expect(result.valid).toBe(true)
    expect(aliasContainerPath("linux")).toBe("/references/linux")
    expect(aliasContainerPath("  ")).toBe("/references/")
  })

  it("rejects aliases that break the protocol pattern", () => {
    const result = validateBindings([
      binding("b1", "Linux"),
      binding("b2", "-linux"),
      binding("b3", ""),
      binding("b4", "a".repeat(defaultReferenceLimits.maxAliasChars + 1)),
    ])

    expect(Object.keys(result.byId)).toEqual(["b1", "b2", "b3", "b4"])
    expect(result.byId.b3.alias).toContain("不能为空")
    expect(result.valid).toBe(false)
  })

  it("rejects duplicate aliases and duplicate sources including disabled rows", () => {
    const result = validateBindings([
      binding("b1", "linux"),
      binding("b2", "linux", { enabled: false }),
      binding("b3", "kernel", { source_id: "source-1" }),
    ])

    expect(result.byId.b2.alias).toContain("唯一")
    expect(result.byId.b3.source).toContain("只能绑定一次")
  })

  it("reports the bindings cap as a general error instead of a row error", () => {
    const rows = Array.from({ length: defaultReferenceLimits.maxBindings + 1 }, (_, index) =>
      binding(`b${index}`, `alias-${index}`))
    const result = validateBindings(rows)

    expect(result.valid).toBe(false)
    expect(result.general).toHaveLength(1)
    expect(result.general[0]).toContain("16")
  })
})

describe("reference draft digest and equality", () => {
  it("ignores row order in the digest but detects content changes", () => {
    const first = [binding("b1", "linux"), binding("b2", "kernel")]
    const reversed = [binding("b2", "kernel"), binding("b1", "linux")]

    expect(draftDigest(first)).toBe(draftDigest(reversed))
    expect(draftDigest(first)).not.toBe(draftDigest([binding("b1", "linux"), binding("b2", "kernel2")]))
  })

  it("treats whitespace-only differences as clean", () => {
    expect(sameBindings([binding("b1", "linux")], [binding("b1", " linux ")])).toBe(true)
    expect(sameBindings([binding("b1", "linux")], [binding("b1", "linux", { enabled: false })])).toBe(false)
  })

  it("generates distinct binding ids", () => {
    expect(newBindingId()).not.toBe(newBindingId())
  })
})

describe("reference source registration validation", () => {
  const draft = { name: "Linux", host_path: "/srv/mai-references/linux/repo", revision_label: "v6.12", approved_for_agent_access: true }

  it("requires an explicit approval and an absolute host path", () => {
    expect(validateSourceDraft(draft)).toEqual({})

    const errors = validateSourceDraft({ ...draft, host_path: "relative/repo", approved_for_agent_access: false })
    expect(errors.host_path).toContain("绝对路径")
    expect(errors.approved_for_agent_access).toBeTruthy()
  })

  it("rejects blank metadata", () => {
    const errors = validateSourceDraft({ name: " ", host_path: "", revision_label: "", approved_for_agent_access: false })
    expect(Object.keys(errors).sort()).toEqual(["approved_for_agent_access", "host_path", "name", "revision_label"])
  })
})
