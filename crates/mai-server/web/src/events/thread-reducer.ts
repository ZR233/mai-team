import type {
  ThreadItem,
  ThreadItemContent,
  ThreadItemDelta,
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
    content: appendDelta(item.content, delta),
  }
  const next = [...items]
  next[index] = updated
  return next
}

function appendDelta(content: ThreadItemContent, delta: ThreadItemDelta): ThreadItemContent {
  switch (delta.field) {
    case "text":
      if (content.type === "userMessage" || content.type === "agentMessage") return { ...content, text: (content.text ?? "") + delta.delta }
      break
    case "reasoning.summary":
      if (content.type === "reasoning") return { ...content, summary: appendChunk(content.summary, delta.delta) }
      break
    case "reasoning.content":
      if (content.type === "reasoning") return { ...content, content: appendChunk(content.content, delta.delta) }
      break
    case "planContent":
      if (content.type === "plan") return { ...content, content: (content.content ?? "") + delta.delta }
      break
    case "tool.arguments":
      if (content.type === "toolCall") return { ...content, tool: { ...content.tool, arguments: (content.tool.arguments ?? "") + delta.delta } }
      break
    case "tool.result":
      if (content.type === "toolCall") return { ...content, tool: { ...content.tool, result: (content.tool.result ?? "") + delta.delta } }
      break
  }
  throw new ThreadProjectionError(`Delta field ${delta.field} is invalid for Item content ${content.type}`)
}

function appendChunk(chunks: string[] | undefined, delta: string): string[] {
  const next = [...(chunks ?? [])]
  if (next.length === 0) next.push(delta)
  else next[next.length - 1] += delta
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
