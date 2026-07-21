import type {
  SessionEventEnvelope,
  SessionEventKind,
  SessionMessage,
  SessionPart,
  SessionStreamFrame,
  SessionViewSnapshot,
} from "@/events/session-events.generated"

export function sessionSnapshot(
  sessionId = "session-a",
  throughSequence = 0,
): SessionViewSnapshot {
  return {
    schemaVersion: 1,
    sessionId,
    throughSequence,
    messages: [],
    parts: [],
    interactions: [],
    agents: [],
    timelineEvents: [],
    activatedSkills: [],
    planEvents: [],
  }
}

export function sessionMessage(sessionId = "session-a"): SessionMessage {
  return {
    messageId: "message-1",
    sessionId,
    turnId: "turn-1",
    role: "assistant",
    status: "streaming",
    createdAt: 1,
    updatedAt: 1,
  }
}

export function textPart(sessionId = "session-a", revision = 0): SessionPart {
  return {
    partId: "part-1",
    messageId: "message-1",
    sessionId,
    turnId: "turn-1",
    order: 0,
    revision,
    status: "streaming",
    createdAt: 1,
    updatedAt: 1,
    content: { type: "text", channel: "final", text: "Hello" },
  }
}

export function durableEvent(
  kind: SessionEventKind,
  sequence: number,
  sessionId = "session-a",
): SessionEventEnvelope {
  return {
    eventId: `${sessionId}:${sequence}`,
    sessionId,
    sourceAgentId: "agent-1",
    turnId: "turn-1",
    emittedAt: sequence,
    position: { persistence: "durable", sequence },
    kind,
  }
}

export function transientEvent(
  kind: SessionEventKind,
  revision: number,
  sessionId = "session-a",
): SessionEventEnvelope {
  return {
    eventId: `${sessionId}:delta:${revision}`,
    sessionId,
    sourceAgentId: "agent-1",
    turnId: "turn-1",
    emittedAt: revision,
    position: { persistence: "transient", revision },
    kind,
  }
}

export function snapshotFrame(snapshot: SessionViewSnapshot): SessionStreamFrame {
  return { type: "snapshot", snapshot }
}
