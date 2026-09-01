import { render, screen, waitFor } from "@testing-library/react"
import userEvent from "@testing-library/user-event"
import { describe, expect, it } from "vitest"

import { TooltipProvider } from "@/components/ui/tooltip"

import { ActiveSkillsStatus } from "./active-skills-status"

describe("ActiveSkillsStatus", () => {
  it("去重显示已加载技能数量，并支持悬停和点击查看完整列表", async () => {
    const user = userEvent.setup()
    render(
      <TooltipProvider>
        <ActiveSkillsStatus skills={["review-single-pr", "rust-code-quality", "review-single-pr"]} />
      </TooltipProvider>,
    )

    const trigger = screen.getByRole("button", { name: "2 skills loaded" })
    expect(trigger).toHaveTextContent("2")

    await user.hover(trigger)
    expect(await screen.findByRole("tooltip")).toHaveTextContent("review-single-pr, rust-code-quality")

    await user.click(trigger)
    await waitFor(() => expect(screen.queryByRole("tooltip")).not.toBeInTheDocument())
    expect(screen.getByRole("menuitemcheckbox", { name: "review-single-pr" })).toHaveAttribute("aria-checked", "true")
    expect(screen.getByRole("menuitemcheckbox", { name: "rust-code-quality" })).toHaveAttribute("aria-checked", "true")
  })

  it("空会话仍显示可发现的零技能状态", async () => {
    const user = userEvent.setup()
    render(
      <TooltipProvider>
        <ActiveSkillsStatus skills={[]} />
      </TooltipProvider>,
    )

    const trigger = screen.getByRole("button", { name: "No skills loaded" })
    expect(trigger).toHaveTextContent("0")
    await user.click(trigger)
    expect(screen.getByText("No skills loaded in this Thread yet.")).toBeVisible()
  })
})
