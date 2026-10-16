import { describe, expect, it } from "vitest"

import type { ProjectReferenceBinding, ProjectReferenceConfiguration } from "@/api/reference-types"

import { ReferenceDraftStore } from "@/features/references/reference-draft-store"

function binding(id: string, alias: string, overrides: Partial<ProjectReferenceBinding> = {}): ProjectReferenceBinding {
  return { id, source_id: "source-1", alias, description: "", enabled: true, ...overrides }
}

function configuration(revision: number, bindings: ProjectReferenceBinding[]): ProjectReferenceConfiguration {
  return { revision, bindings }
}

describe("reference draft ownership", () => {
  it("keeps a separate draft per project and preserves edits when switching projects", () => {
    const store = new ReferenceDraftStore()
    store.ensureFromServer("project-a", configuration(0, [binding("b1", "linux")]))
    store.ensureFromServer("project-b", configuration(0, [binding("b2", "qemu")]))

    store.setBindings("project-a", [binding("b1", "kernel")])

    expect(store.peek("project-a")?.draft[0].alias).toBe("kernel")
    expect(store.peek("project-b")?.draft[0].alias).toBe("qemu")
    expect(store.hasDirtyDraft("project-a")).toBe(true)
    expect(store.hasDirtyDraft("project-b")).toBe(false)
  })

  it("never resets a dirty draft from a server refresh", () => {
    const store = new ReferenceDraftStore()
    store.ensureFromServer("project-a", configuration(0, [binding("b1", "linux")]))
    store.setBindings("project-a", [binding("b1", "kernel")])

    store.syncFromServer("project-a", configuration(4, [binding("b1", "linux"), binding("b2", "extra")]))

    const draft = store.peek("project-a")
    expect(draft?.draft.map((row) => row.alias)).toEqual(["kernel"])
    // 基线仍是用户编辑时的版本：保存会 CAS 失败，而不是静默覆盖服务器上的新版本。
    expect(draft?.baseRevision).toBe(0)
  })

  it("adopts the server configuration while the draft is clean", () => {
    const store = new ReferenceDraftStore()
    store.ensureFromServer("project-a", configuration(0, [binding("b1", "linux")]))

    store.syncFromServer("project-a", configuration(2, [binding("b1", "linux-2")]))

    expect(store.peek("project-a")?.draft[0].alias).toBe("linux-2")
    expect(store.peek("project-a")?.baseRevision).toBe(2)
  })

  it("keeps the same snapshot object while the server repeats the same configuration", () => {
    const store = new ReferenceDraftStore()
    store.ensureFromServer("project-a", configuration(2, [binding("b1", "linux")]))
    const before = store.peek("project-a")

    // refetch / SSE 触发相同事实：不能产生新快照，否则卸载重开会丢失稳定身份。
    store.syncFromServer("project-a", configuration(2, [binding("b1", "linux")]))

    expect(store.peek("project-a")).toBe(before)
  })

  it("ignores a stale server configuration older than the known baseline", () => {
    const store = new ReferenceDraftStore()
    store.ensureFromServer("project-a", configuration(3, [binding("b1", "linux")]))

    store.syncFromServer("project-a", configuration(1, [binding("b1", "old")]))

    expect(store.peek("project-a")?.draft[0].alias).toBe("linux")
    expect(store.peek("project-a")?.baseRevision).toBe(3)
  })
})

describe("reference draft saving", () => {
  it("treats a double click as one request", () => {
    const store = new ReferenceDraftStore()
    store.ensureFromServer("project-a", configuration(0, [binding("b1", "linux")]))
    store.setBindings("project-a", [binding("b1", "kernel")])

    expect(store.beginSave("project-a")).toBe(1)
    expect(store.beginSave("project-a")).toBeNull()
    expect(store.peek("project-a")?.saving).toBe(true)
  })

  it("merges edits made while the save was in flight", () => {
    const store = new ReferenceDraftStore()
    store.ensureFromServer("project-a", configuration(0, [binding("b1", "linux")]))
    store.setBindings("project-a", [binding("b1", "kernel")])

    const sequence = store.beginSave("project-a")
    store.setBindings("project-a", [binding("b1", "kernel"), binding("b2", "next")])
    store.saveSucceeded("project-a", sequence!, configuration(1, [binding("b1", "kernel")]))

    const draft = store.peek("project-a")
    expect(draft?.baseRevision).toBe(1)
    expect(draft?.original.map((row) => row.alias)).toEqual(["kernel"])
    expect(draft?.draft.map((row) => row.alias)).toEqual(["kernel", "next"])
    expect(draft?.saving).toBe(false)
  })

  it("ignores results from a superseded request sequence", () => {
    const store = new ReferenceDraftStore()
    store.ensureFromServer("project-a", configuration(0, [binding("b1", "linux")]))
    store.setBindings("project-a", [binding("b1", "kernel")])
    const sequence = store.beginSave("project-a")

    store.saveFailed("project-a", sequence! - 1, { message: "stale" })
    expect(store.peek("project-a")?.saving).toBe(true)
    expect(store.peek("project-a")?.error).toBeNull()

    store.saveFailed("project-a", sequence!, { message: "conflict", conflict: { expectedRevision: 0, serverRevision: 3, message: "conflict", requestId: "req-1" } })
    expect(store.peek("project-a")?.saving).toBe(false)
    expect(store.peek("project-a")?.conflict?.serverRevision).toBe(3)
  })

  it("scopes late responses to their own project", () => {
    const store = new ReferenceDraftStore()
    store.ensureFromServer("project-a", configuration(0, [binding("b1", "linux")]))
    store.ensureFromServer("project-b", configuration(0, [binding("b2", "qemu")]))
    store.setBindings("project-a", [binding("b1", "kernel")])
    store.setBindings("project-b", [binding("b2", "firmware")])

    const sequenceA = store.beginSave("project-a")
    const sequenceB = store.beginSave("project-b")

    // project-a 的失败只收敛 project-a：另一个项目的保存仍在进行，草稿不受影响。
    store.saveFailed("project-a", sequenceA!, { message: "conflict", conflict: { expectedRevision: 0, serverRevision: 4, message: "conflict", requestId: "req-a" } })

    expect(store.peek("project-a")?.saving).toBe(false)
    expect(store.peek("project-a")?.conflict?.requestId).toBe("req-a")
    expect(store.peek("project-b")?.saving).toBe(true)
    expect(store.peek("project-b")?.conflict).toBeNull()
    expect(store.peek("project-b")?.draft[0].alias).toBe("firmware")

    // 晚到的 project-b 结果按它自己的序号生效。
    store.saveSucceeded("project-b", sequenceB!, configuration(1, [binding("b2", "firmware")]))
    expect(store.hasDirtyDraft("project-b")).toBe(false)
    expect(store.hasDirtyDraft("project-a")).toBe(true)
  })

  it("keeps in-flight edits visible when the same project is read again", () => {
    const store = new ReferenceDraftStore()
    store.ensureFromServer("project-a", configuration(0, [binding("b1", "linux")]))
    store.setBindings("project-a", [binding("b1", "kernel")])
    const sequence = store.beginSave("project-a")
    store.setBindings("project-a", [binding("b1", "kernel2")])

    // 组件卸载/重新挂载：重新读取同一项目必须拿到带飞行中编辑的稳定快照。
    const reopened = store.peek("project-a")
    expect(reopened?.draft[0].alias).toBe("kernel2")
    expect(reopened?.saving).toBe(true)
    store.syncFromServer("project-a", configuration(0, [binding("b1", "linux")]))

    expect(store.peek("project-a")).toBe(reopened)
    expect(store.peek("project-a")?.draft[0].alias).toBe("kernel2")

    store.saveSucceeded("project-a", sequence!, configuration(1, [binding("b1", "kernel")]))
    expect(store.peek("project-a")?.baseRevision).toBe(1)
    expect(store.peek("project-a")?.draft[0].alias).toBe("kernel2")
    expect(store.hasDirtyDraft("project-a")).toBe(true)
  })

  it("keeps the draft after a failure and clears the error on success", () => {
    const store = new ReferenceDraftStore()
    store.ensureFromServer("project-a", configuration(0, [binding("b1", "linux")]))
    store.setBindings("project-a", [binding("b1", "kernel")])

    const first = store.beginSave("project-a")
    store.saveFailed("project-a", first!, { message: "network down" })
    expect(store.peek("project-a")?.draft[0].alias).toBe("kernel")
    expect(store.peek("project-a")?.error).toBe("network down")

    const second = store.beginSave("project-a")
    store.saveSucceeded("project-a", second!, configuration(1, [binding("b1", "kernel")]))
    expect(store.peek("project-a")?.error).toBeNull()
    expect(store.hasDirtyDraft("project-a")).toBe(false)
  })

  it("discards local edits only when the user explicitly reloads the server version", () => {
    const store = new ReferenceDraftStore()
    store.ensureFromServer("project-a", configuration(0, [binding("b1", "linux")]))
    store.setBindings("project-a", [binding("b1", "kernel")])

    store.dismissConflict("project-a")
    expect(store.peek("project-a")?.draft[0].alias).toBe("kernel")

    store.resetToServer("project-a", configuration(3, [binding("b9", "linux")]))
    expect(store.peek("project-a")?.draft.map((row) => row.id)).toEqual(["b9"])
    expect(store.peek("project-a")?.baseRevision).toBe(3)
    expect(store.hasDirtyDraft("project-a")).toBe(false)
  })
})
