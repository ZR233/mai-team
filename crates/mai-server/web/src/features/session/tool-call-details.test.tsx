import { render, screen } from "@testing-library/react"
import userEvent from "@testing-library/user-event"
import { describe, expect, it } from "vitest"

import { ToolCallDetails } from "./tool-call-details"

describe("tool call details", () => {
  it("shows semantic content by default and keeps raw JSON behind a second disclosure", async () => {
    render(<ToolCallDetails defaultOpen tool={{
      name: "exec",
      status: "completed",
      arguments: '{"command":"printf hello","cwd":"/workspace"}',
      result: '{"status":"completed","exitCode":0,"stdout":"hello","stderr":""}',
    }} />)

    expect(screen.getByText("Command")).toBeInTheDocument()
    expect(screen.getAllByText("printf hello")).toHaveLength(2)
    expect(screen.getByText("Standard output")).toBeInTheDocument()
    expect(screen.getByText("hello")).toBeInTheDocument()
    expect(screen.queryByText(/"command"/)).not.toBeInTheDocument()

    await userEvent.click(screen.getByRole("button", { name: "Show original tool data" }))

    expect(screen.getByText(/"command": "printf hello"/)).toBeInTheDocument()
    expect(screen.getByRole("button", { name: "Hide original tool data" })).toHaveAttribute("aria-expanded", "true")
  })

  it("keeps a concise accessible trigger when collapsed", () => {
    render(<ToolCallDetails tool={{ name: "exec", status: "failed", exitCode: 2, arguments: '{"command":"cargo check"}' }} />)

    expect(screen.getByRole("button", { name: "Expand Run command" })).toHaveAttribute("aria-expanded", "false")
    expect(screen.getByText("cargo check")).toBeInTheDocument()
    expect(screen.getByText("exit 2")).toBeInTheDocument()
    expect(screen.queryByText("Command")).not.toBeInTheDocument()
  })
})
