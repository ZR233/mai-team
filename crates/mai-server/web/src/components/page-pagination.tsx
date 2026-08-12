import { ChevronLeft, ChevronRight, ChevronsLeft, ChevronsRight } from "lucide-react"

import { Button } from "@/components/ui/button"

interface PagePaginationProps {
  page: number
  totalPages: number
  onPageChange(page: number): void
  disabled?: boolean
  label?: string
}

export function PagePagination({ page, totalPages, onPageChange, disabled = false, label = "Pagination" }: PagePaginationProps) {
  if (totalPages <= 1) return null
  const changePage = (next: number) => {
    if (!disabled && next >= 1 && next <= totalPages && next !== page) onPageChange(next)
  }
  return <nav aria-label={label} className="flex items-center justify-between gap-2">
    <div className="flex w-full items-center justify-between gap-2 sm:hidden">
      <Button type="button" variant="outline" size="sm" disabled={disabled || page <= 1} onClick={() => changePage(page - 1)}><ChevronLeft /> Previous</Button>
      <span className="text-xs tabular-nums text-muted-foreground">Page {page} of {totalPages}</span>
      <Button type="button" variant="outline" size="sm" disabled={disabled || page >= totalPages} onClick={() => changePage(page + 1)}>Next <ChevronRight /></Button>
    </div>
    <div className="hidden w-full items-center justify-center gap-1 sm:flex">
      <Button type="button" variant="outline" size="icon-sm" aria-label="First page" disabled={disabled || page <= 1} onClick={() => changePage(1)}><ChevronsLeft /></Button>
      <Button type="button" variant="outline" size="icon-sm" aria-label="Previous page" disabled={disabled || page <= 1} onClick={() => changePage(page - 1)}><ChevronLeft /></Button>
      {paginationWindow(page, totalPages).map((number) => <Button key={number} type="button" variant={number === page ? "default" : "outline"} size="icon-sm" aria-label={`Page ${number}`} aria-current={number === page ? "page" : undefined} disabled={disabled} onClick={() => changePage(number)}>{number}</Button>)}
      <Button type="button" variant="outline" size="icon-sm" aria-label="Next page" disabled={disabled || page >= totalPages} onClick={() => changePage(page + 1)}><ChevronRight /></Button>
      <Button type="button" variant="outline" size="icon-sm" aria-label="Last page" disabled={disabled || page >= totalPages} onClick={() => changePage(totalPages)}><ChevronsRight /></Button>
    </div>
  </nav>
}

export function paginationWindow(page: number, totalPages: number) {
  const start = Math.max(1, Math.min(page - 2, totalPages - 4))
  const end = Math.min(totalPages, Math.max(page + 2, 5))
  return Array.from({ length: end - start + 1 }, (_, index) => start + index)
}
