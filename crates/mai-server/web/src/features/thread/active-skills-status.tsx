import { Sparkles } from "lucide-react"
import { useMemo, useState } from "react"

import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"

export function ActiveSkillsStatus({ skills }: { skills: readonly string[] }) {
  const activeSkills = useMemo(() => [...new Set(skills)], [skills])
  const [menuOpen, setMenuOpen] = useState(false)
  const [tooltipOpen, setTooltipOpen] = useState(false)
  const count = activeSkills.length
  const accessibleLabel = count === 0 ? "No skills loaded" : `${count} skill${count === 1 ? "" : "s"} loaded`
  const tooltipLabel = count === 0
    ? "No skills loaded in this Thread yet."
    : `Loaded skills: ${activeSkills.join(", ")}`

  return (
    <DropdownMenu
      open={menuOpen}
      onOpenChange={(open: boolean) => {
        setMenuOpen(open)
        if (open) setTooltipOpen(false)
      }}
    >
      <Tooltip open={tooltipOpen && !menuOpen} onOpenChange={setTooltipOpen}>
        <TooltipTrigger asChild>
          <DropdownMenuTrigger asChild>
            <Button aria-label={accessibleLabel} variant="ghost" size="xs">
              <Sparkles data-icon="inline-start" />
              <span className="hidden sm:inline">Skills</span>
              <Badge variant="secondary">{count}</Badge>
            </Button>
          </DropdownMenuTrigger>
        </TooltipTrigger>
        <TooltipContent>{tooltipLabel}</TooltipContent>
      </Tooltip>
      <DropdownMenuContent align="end" className="w-64">
        <DropdownMenuLabel>Loaded skills</DropdownMenuLabel>
        <DropdownMenuGroup>
          {count === 0
            ? <DropdownMenuItem disabled>No skills loaded in this Thread yet.</DropdownMenuItem>
            : activeSkills.map((skill) => (
              <DropdownMenuCheckboxItem
                checked
                key={skill}
                onSelect={(event: Event) => event.preventDefault()}
              >
                {skill}
              </DropdownMenuCheckboxItem>
            ))}
        </DropdownMenuGroup>
      </DropdownMenuContent>
    </DropdownMenu>
  )
}
