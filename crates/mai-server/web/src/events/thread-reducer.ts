import type {
  InteractionRequest,
  ThreadNotificationEnvelope,
  ThreadSnapshot,
} from "@/events/thread-events.generated"

/**
 * Thread 投影错误：权威首帧或 typed 通知无法拼成连续视图。
 *
 * 任何不一致（Thread 身份、水位缺口、跨 Thread 载荷）都直接抛出而不是降级拼接，
 * 由事件控制器重新订阅并回到权威首帧。
 */
export class ThreadProjectionError extends Error {}

/**
 * 校验权威首帧属于目标 Thread，并逐项校验它携带的 typed 载荷所有权。
 *
 * 首帧是唯一的权威状态来源：历史 Turn/items 不在这里，由 `/threads/{id}/turns`
 * 的 typed page 单独读取。
 */
export function validateThreadSnapshot(threadId: string, snapshot: ThreadSnapshot): ThreadSnapshot {
  if (snapshot.thread.id !== threadId) {
    throw new ThreadProjectionError(`Thread snapshot mismatch: expected ${threadId}, got ${snapshot.thread.id}`)
  }
  if (snapshot.activeTurn && snapshot.activeTurn.threadId !== threadId) {
    throw new ThreadProjectionError(`Active Turn belongs to Thread ${snapshot.activeTurn.threadId}, expected ${threadId}`)
  }
  if (snapshot.runtime && snapshot.runtime.threadId !== threadId) {
    throw new ThreadProjectionError(`Runtime snapshot belongs to Thread ${snapshot.runtime.threadId}, expected ${threadId}`)
  }
  for (const interaction of snapshot.interactions) validateInteractionOwner(threadId, interaction)
  return snapshot
}

/**
 * 把一条 typed 通知拼接到当前权威快照上。
 *
 * 通知只维护 Thread 级状态：Turn 生命周期、interaction、runtime、activity 与 storage。
 * 它不携带历史 Turn/items，因此这里既不伪造也不追加历史条目。
 *
 * `envelope.baseRevision` 必须等于当前水位、`envelope.revision` 必须恰好 +1；任何缺口都
 * 说明视图有洞，直接失败让调用方重同步。
 */
export function applyThreadNotification(
  current: ThreadSnapshot,
  envelope: ThreadNotificationEnvelope,
): ThreadSnapshot {
  const threadId = current.thread.id
  if (envelope.threadId !== threadId) {
    throw new ThreadProjectionError(`Thread notification mismatch: expected ${threadId}, got ${envelope.threadId}`)
  }

  const notification = envelope.notification
  if (notification.type === "lagged") {
    throw new ThreadProjectionError(`Thread subscription lagged by ${notification.dropped} notifications`)
  }
  if (envelope.baseRevision !== current.revision) {
    throw new ThreadProjectionError(`Thread revision gap: expected base ${current.revision}, got ${envelope.baseRevision}`)
  }
  const expected = current.revision + 1
  if (envelope.revision !== expected) {
    throw new ThreadProjectionError(`Thread revision must advance by one: expected ${expected}, got ${envelope.revision}`)
  }

  const next: ThreadSnapshot = { ...current, revision: envelope.revision }
  switch (notification.type) {
    case "turnStarted":
    case "turnUpdated":
      validateTurnOwner(threadId, notification.turn.threadId)
      return { ...next, activeTurn: notification.turn }
    case "turnCompleted":
      validateTurnOwner(threadId, notification.turn.threadId)
      return next.activeTurn?.id === notification.turn.id ? { ...next, activeTurn: undefined } : next
    case "interactionChanged":
      validateInteractionOwner(threadId, notification.interaction)
      return { ...next, interactions: upsertInteraction(next.interactions, notification.interaction) }
    case "threadRuntimeUpdated":
      if (notification.runtime.threadId !== threadId) {
        throw new ThreadProjectionError(`Runtime snapshot belongs to Thread ${notification.runtime.threadId}, expected ${threadId}`)
      }
      return { ...next, runtime: notification.runtime }
    case "activityChanged":
      return { ...next, activity: notification.activity ?? undefined }
    case "storageChanged":
      return { ...next, storage: notification.storage ?? undefined }
  }
}

function upsertInteraction(interactions: InteractionRequest[], incoming: InteractionRequest): InteractionRequest[] {
  const index = interactions.findIndex((entry) => entry.interactionId === incoming.interactionId)
  if (index < 0) return [...interactions, incoming]
  const current = interactions[index]
  if (incoming.revision < current.revision) {
    throw new ThreadProjectionError(`Interaction ${incoming.interactionId} revision regressed from ${current.revision} to ${incoming.revision}`)
  }
  const updated = [...interactions]
  updated[index] = incoming
  return updated
}

function validateInteractionOwner(threadId: string, interaction: InteractionRequest) {
  if (interaction.scope.threadId !== threadId) {
    throw new ThreadProjectionError(`Interaction ${interaction.interactionId} belongs to Thread ${interaction.scope.threadId}, expected ${threadId}`)
  }
}

function validateTurnOwner(threadId: string, actual: string) {
  if (actual !== threadId) throw new ThreadProjectionError(`Turn belongs to Thread ${actual}, expected ${threadId}`)
}
