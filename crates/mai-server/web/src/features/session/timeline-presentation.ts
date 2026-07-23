import type { SessionPart } from "@/events/session-events.generated"

export type ToolSessionPart = SessionPart & {
  content: Extract<SessionPart["content"], { type: "tool" }>
}

export type TimelineRenderItem =
  | { kind: "part"; key: string; part: SessionPart }
  | { kind: "toolGroup"; key: string; groupId: string; parts: ToolSessionPart[] }

export function buildTimelineRenderItems(parts: SessionPart[]): TimelineRenderItem[] {
  const items: TimelineRenderItem[] = []

  for (let index = 0; index < parts.length; index += 1) {
    const part = parts[index]
    const groupId = toolActivityGroupId(part)
    if (!groupId) {
      items.push({ kind: "part", key: part.partId, part })
      continue
    }

    const groupedParts = [part as ToolSessionPart]
    while (index + 1 < parts.length && toolActivityGroupId(parts[index + 1]) === groupId) {
      index += 1
      groupedParts.push(parts[index] as ToolSessionPart)
    }

    appendPendingGroup(items, { groupId, parts: groupedParts })
  }

  return items
}

function appendPendingGroup(
  items: TimelineRenderItem[],
  pendingGroup: { groupId: string; parts: ToolSessionPart[] },
) {
  if (pendingGroup.parts.length === 1) {
    const [part] = pendingGroup.parts
    items.push({ kind: "part", key: part.partId, part })
    return
  }

  const firstPartId = pendingGroup.parts[0].partId
  items.push({
    kind: "toolGroup",
    key: `tool-group:${pendingGroup.groupId}:${firstPartId}`,
    groupId: pendingGroup.groupId,
    parts: pendingGroup.parts,
  })
}

function toolActivityGroupId(part: SessionPart): string | null {
  if (part.content.type !== "tool") return null
  return part.content.tool.activityGroupId || null
}
