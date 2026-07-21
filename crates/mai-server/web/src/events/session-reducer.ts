import type {
  SessionEventEnvelope,
  SessionPart,
  SessionPartDelta,
  SessionViewSnapshot,
} from "@/events/session-events.generated"

export interface NormalizedSession {
  sessionId: string | null
  throughSequence: number
  turn: SessionViewSnapshot["turn"]
  messages: Record<string, SessionViewSnapshot["messages"][number]>
  messageOrder: string[]
  parts: Record<string, SessionPart>
  partOrderByMessage: Record<string, string[]>
  interactions: SessionViewSnapshot["interactions"]
  agents: Record<string, SessionViewSnapshot["agents"][number]>
  timelineEvents: SessionViewSnapshot["timelineEvents"]
  runtime: SessionViewSnapshot["runtime"]
  activatedSkills: SessionViewSnapshot["activatedSkills"]
  planEvents: SessionViewSnapshot["planEvents"]
  lastError: string | null
}

export function emptySession(): NormalizedSession {
  return {
    sessionId: null,
    throughSequence: 0,
    turn: undefined,
    messages: {},
    messageOrder: [],
    parts: {},
    partOrderByMessage: {},
    interactions: [],
    agents: {},
    timelineEvents: [],
    runtime: undefined,
    activatedSkills: [],
    planEvents: [],
    lastError: null,
  }
}

export function normalizeSnapshot(snapshot: SessionViewSnapshot): NormalizedSession {
  const messages = Object.fromEntries(snapshot.messages.map((message) => [message.messageId, message]))
  const parts = Object.fromEntries(snapshot.parts.map((part) => [part.partId, part]))
  const partOrderByMessage: Record<string, string[]> = {}
  for (const part of snapshot.parts) {
    const order = partOrderByMessage[part.messageId] ?? []
    order.push(part.partId)
    partOrderByMessage[part.messageId] = order
  }
  for (const order of Object.values(partOrderByMessage)) {
    order.sort((left, right) => (parts[left]?.order ?? 0) - (parts[right]?.order ?? 0))
  }
  return {
    sessionId: snapshot.sessionId,
    throughSequence: snapshot.throughSequence,
    turn: snapshot.turn,
    messages,
    messageOrder: snapshot.messages.map((message) => message.messageId),
    parts,
    partOrderByMessage,
    interactions: snapshot.interactions,
    agents: Object.fromEntries(snapshot.agents.map((agent) => [agent.id, agent])),
    timelineEvents: snapshot.timelineEvents,
    runtime: snapshot.runtime,
    activatedSkills: snapshot.activatedSkills,
    planEvents: snapshot.planEvents,
    lastError: null,
  }
}

export function applySessionEvent(current: NormalizedSession, event: SessionEventEnvelope): NormalizedSession {
  if (current.sessionId && current.sessionId !== event.sessionId) return current
  const durableSequence = event.position.persistence === "durable" ? event.position.sequence : null
  if (durableSequence !== null && durableSequence <= current.throughSequence) return current
  if (durableSequence !== null && durableSequence !== current.throughSequence + 1) {
    throw new Error(`session sequence gap: expected ${current.throughSequence + 1}, got ${durableSequence}`)
  }

  const next: NormalizedSession = {
    ...current,
    sessionId: event.sessionId,
    throughSequence: durableSequence ?? current.throughSequence,
  }
  const kind = event.kind
  switch (kind.type) {
    case "turnChanged":
      return { ...next, turn: kind.turn }
    case "messageChanged": {
      const exists = Boolean(next.messages[kind.message.messageId])
      return {
        ...next,
        messages: { ...next.messages, [kind.message.messageId]: kind.message },
        messageOrder: exists ? next.messageOrder : [...next.messageOrder, kind.message.messageId],
      }
    }
    case "messageRemoved": {
      const messages = { ...next.messages }
      delete messages[kind.messageId]
      return {
        ...next,
        messages,
        messageOrder: next.messageOrder.filter((id) => id !== kind.messageId),
      }
    }
    case "partChanged":
      return upsertPart(next, kind.part)
    case "partRemoved": {
      const parts = { ...next.parts }
      delete parts[kind.partId]
      return {
        ...next,
        parts,
        partOrderByMessage: {
          ...next.partOrderByMessage,
          [kind.messageId]: (next.partOrderByMessage[kind.messageId] ?? []).filter((id) => id !== kind.partId),
        },
      }
    }
    case "partDelta":
      return applyPartDelta(next, kind.delta)
    case "interactionChanged":
      return {
        ...next,
        interactions: kind.event.interaction.status === "pending"
          ? upsertByKey(
              next.interactions,
              kind.event.interaction,
              (interaction) => interaction.interactionId,
            )
          : next.interactions.filter(
              (interaction) => interaction.interactionId !== kind.event.interaction.interactionId,
            ),
      }
    case "agentChanged": {
      const agents = { ...next.agents, [kind.agent.id]: kind.agent }
      return {
        ...next,
        agents,
        runtime: next.runtime
          ? { ...next.runtime, agentCount: Object.keys(agents).length }
          : next.runtime,
      }
    }
    case "timelineEventAppended":
      return { ...next, timelineEvents: [...next.timelineEvents, kind.event] }
    case "runtimeChanged":
      return { ...next, runtime: kind.runtime }
    case "skillActivated": {
      const activeSkills = next.runtime && !next.runtime.activeSkills.includes(kind.activation.name)
        ? [...next.runtime.activeSkills, kind.activation.name]
        : next.runtime?.activeSkills
      return {
        ...next,
        runtime: next.runtime && activeSkills
          ? { ...next.runtime, activeSkills }
          : next.runtime,
        activatedSkills: upsertByName(next.activatedSkills, kind.activation),
      }
    }
    case "planChanged":
      return { ...next, planEvents: [...next.planEvents, kind.event] }
    case "contextCompacted":
      return next
    case "errorOccurred":
      return { ...next, lastError: kind.message }
    default:
      return next
  }
}

function upsertPart(current: NormalizedSession, part: SessionPart): NormalizedSession {
  const exists = Boolean(current.parts[part.partId])
  const order = current.partOrderByMessage[part.messageId] ?? []
  const nextOrder = exists ? order : [...order, part.partId]
  nextOrder.sort((left, right) => {
    const leftPart = left === part.partId ? part : current.parts[left]
    const rightPart = right === part.partId ? part : current.parts[right]
    return (leftPart?.order ?? 0) - (rightPart?.order ?? 0)
  })
  return {
    ...current,
    parts: { ...current.parts, [part.partId]: part },
    partOrderByMessage: { ...current.partOrderByMessage, [part.messageId]: nextOrder },
  }
}

function applyPartDelta(current: NormalizedSession, delta: SessionPartDelta): NormalizedSession {
  const part = current.parts[delta.partId]
  if (!part) throw new Error(`part delta targets unknown part ${delta.partId}`)
  if (delta.revision <= part.revision) return current
  if (delta.revision !== part.revision + 1) {
    throw new Error(`part ${delta.partId} revision gap: expected ${part.revision + 1}, got ${delta.revision}`)
  }
  const content = structuredClone(part.content)
  switch (delta.field) {
    case "text":
      if (content.type !== "text") throw new Error("text delta targets non-text part")
      content.text = `${content.text ?? ""}${delta.delta}`
      break
    case "reasoning.summary":
      if (content.type !== "reasoning") throw new Error("reasoning delta targets non-reasoning part")
      content.text = `${content.text ?? ""}${delta.delta}`
      break
    case "planContent":
      if (content.type !== "plan") throw new Error("plan delta targets non-plan part")
      content.content = `${content.content ?? ""}${delta.delta}`
      break
    case "tool.arguments":
      if (content.type !== "tool") throw new Error("tool arguments delta targets non-tool part")
      content.tool.arguments = `${content.tool.arguments ?? ""}${delta.delta}`
      break
    case "tool.result":
      if (content.type !== "tool") throw new Error("tool result delta targets non-tool part")
      content.tool.result = `${content.tool.result ?? ""}${delta.delta}`
      break
  }
  return upsertPart(current, { ...part, revision: delta.revision, content })
}

function upsertByKey<T>(items: T[], item: T, key: (item: T) => string): T[] {
  const itemKey = key(item)
  const index = items.findIndex((candidate) => key(candidate) === itemKey)
  if (index < 0) return [...items, item]
  return items.map((candidate, candidateIndex) => candidateIndex === index ? item : candidate)
}

function upsertByName<T extends { name: string }>(items: T[], item: T): T[] {
  return [...items.filter((candidate) => candidate.name !== item.name), item]
}
