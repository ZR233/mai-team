import { render, screen } from "@testing-library/react"
import userEvent from "@testing-library/user-event"
import { describe, expect, it, vi } from "vitest"

import { PagePagination, paginationWindow } from "./page-pagination"

describe("page pagination", () => {
  it("keeps a bounded page window around the current page", () => {
    expect(paginationWindow(1, 12)).toEqual([1, 2, 3, 4, 5])
    expect(paginationWindow(7, 12)).toEqual([5, 6, 7, 8, 9])
    expect(paginationWindow(12, 12)).toEqual([8, 9, 10, 11, 12])
  })

  it("supports desktop and mobile navigation controls", async () => {
    const onPageChange = vi.fn()
    render(<PagePagination page={3} totalPages={7} onPageChange={onPageChange} />)

    await userEvent.click(screen.getByRole("button", { name: "First page" }))
    await userEvent.click(screen.getByRole("button", { name: "Next page" }))
    await userEvent.click(screen.getByRole("button", { name: /^previous$/i }))

    expect(onPageChange.mock.calls).toEqual([[1], [4], [2]])
    expect(screen.getByText("Page 3 of 7")).toBeInTheDocument()
    expect(screen.getByRole("button", { name: "Page 3" })).toHaveAttribute("aria-current", "page")
  })
})
