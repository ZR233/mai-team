import type {
  ThreadItem,
  ThreadItemDelta,
  ThreadItemState,
  ThreadNotificationEnvelope,
  ThreadSnapshot,
} from "@/events/thread-events.generated"

export class ThreadProjectionError extends Error {}

export function validateThreadSnapshot(threadId: string, snapshot: ThreadSnapshot): ThreadSnapshot {
  if (snapshot.thread.id !== threadId) {
    throw new ThreadProjectionError(`Thread snapshot mismatch: expected ${threadId}, got ${snapshot.thread.id}`)
  }
  for (const item of snapshot.items) validateItemOwner(threadId, item)
  return { ...snapshot, items: [...snapshot.items].sort(compareItems) }
}

export function applyThreadNotification(
  current: ThreadSnapshot,
  envelope: ThreadNotificationEnvelope,
): ThreadSnapshot {
  const threadId = current.thread.id
  if (envelope.threadId !== threadId) {
    throw new ThreadProjectionError(`Thread notification mismatch: expected ${threadId}, got ${envelope.threadId}`)
  }
  const expected = current.revision + 1
  if (envelope.revision !== expected) {
    throw new ThreadProjectionError(`Thread revision gap: expected ${expected}, got ${envelope.revision}`)
  }

  const notification = envelope.notification
  if (notification.type === "lagged") {
    throw new ThreadProjectionError(`Thread subscription lagged by ${notification.dropped} notifications`)
  }

  let next: ThreadSnapshot = { ...current, revision: envelope.revision }
  switch (notification.type) {
    case "turnStarted":
    case "turnUpdated":
      validateTurnOwner(threadId, notification.turn.threadId)
      next = { ...next, activeTurn: notification.turn }
      break
    case "turnCompleted":
      validateTurnOwner(threadId, notification.turn.threadId)
      next = { ...next, activeTurn: undefined }
      break
    case "itemStarted":
    case "itemCompleted":
      next = { ...next, items: upsertItem(next.items, threadId, notification.item) }
      break
    case "itemDelta":
      next = { ...next, items: applyItemDelta(next.items, notification.delta, envelope.emittedAt) }
      break
    case "interactionChanged": {
      const id = String(notification.interaction.interactionId ?? "")
      const interactions = next.interactions.filter((entry) => String(entry.interactionId ?? "") !== id)
      next = { ...next, interactions: [...interactions, notification.interaction] }
      break
    }
    case "threadRuntimeUpdated":
      validateTurnOwner(threadId, notification.runtime.threadId)
      next = { ...next, runtime: notification.runtime }
      break
  }
  return next
}

function upsertItem(items: ThreadItem[], threadId: string, incoming: ThreadItem): ThreadItem[] {
  validateItemOwner(threadId, incoming)
  const current = items.find((item) => item.id === incoming.id)
  if (current) {
    if (current.threadId !== incoming.threadId || current.turnId !== incoming.turnId) {
      throw new ThreadProjectionError(`Item ${incoming.id} crossed Thread or Turn ownership`)
    }
    if (incoming.revision < current.revision) {
      throw new ThreadProjectionError(`Item ${incoming.id} revision regressed from ${current.revision} to ${incoming.revision}`)
    }
  }
  return [...items.filter((item) => item.id !== incoming.id), incoming].sort(compareItems)
}

function applyItemDelta(items: ThreadItem[], delta: ThreadItemDelta, emittedAt: number): ThreadItem[] {
  const index = items.findIndex((item) => item.id === delta.itemId)
  if (index < 0) throw new ThreadProjectionError(`Delta references missing Item ${delta.itemId}`)
  const item = items[index]
  const expected = item.revision + 1
  if (delta.revision !== expected) {
    throw new ThreadProjectionError(`Item ${item.id} revision gap: expected ${expected}, got ${delta.revision}`)
  }
  const updated = {
    ...item,
    revision: delta.revision,
    updatedAt: emittedAt,
    state: appendDelta(item.state, delta),
  }
  const next = [...items]
  next[index] = updated
  return next
}

function appendDelta(state: ThreadItemState, delta: ThreadItemDelta): ThreadItemState {
  const change = delta.delta
  switch (change.kind) {
    case "text":
      if (state.kind === "text") return { ...state, data: { ...state.data, text: state.data.text + change.data.delta } }
      break
    case "thinkingSummary":
      if (state.kind === "thinking") return { ...state, data: { ...state.data, summary: appendChunk(state.data.summary, change.data.chunkIndex, change.data.delta) } }
      break
    case "thinkingContent":
      if (state.kind === "thinking") return { ...state, data: { ...state.data, content: appendChunk(state.data.content, change.data.chunkIndex, change.data.delta) } }
      break
    case "plan":
      if (state.kind === "plan") return { ...state, data: { ...state.data, content: state.data.content + change.data.delta } }
      break
    case "toolArguments":
      if (state.kind === "tool") {
        return {
          ...state,
          data: {
            invocation: { ...state.data.invocation, arguments: (state.data.invocation.arguments ?? "") + change.data.delta },
            state: { kind: "streaming", data: null },
          },
        }
      }
      break
    case "toolResult":
      if (state.kind === "tool" && state.data.state.kind === "running") {
        return {
          ...state,
          data: {
            ...state.data,
            state: { kind: "running", data: { streamedOutput: (state.data.state.data.streamedOutput ?? "") + change.data.delta } },
          },
        }
      }
      break
  }
  throw new ThreadProjectionError(`Delta ${change.kind} is invalid for Item state ${state.kind}`)
}

function appendChunk(chunks: string[] | undefined, chunkIndex: number, delta: string): string[] {
  const next = [...(chunks ?? [])]
  if (chunkIndex > next.length) throw new ThreadProjectionError(`Delta skipped reasoning chunk ${next.length}`)
  if (chunkIndex === next.length) next.push("")
  next[chunkIndex] += delta
  return next
}

function validateItemOwner(threadId: string, item: ThreadItem) {
  if (item.threadId !== threadId) throw new ThreadProjectionError(`Item ${item.id} belongs to Thread ${item.threadId}`)
}

function validateTurnOwner(threadId: string, actual: string) {
  if (actual !== threadId) throw new ThreadProjectionError(`Turn belongs to Thread ${actual}, expected ${threadId}`)
}

function compareItems(left: ThreadItem, right: ThreadItem) {
  return left.ordinal - right.ordinal || left.id.localeCompare(right.id)
}
