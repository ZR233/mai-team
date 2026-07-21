import { Menu, Plus } from "lucide-react"

import { Avatar, AvatarFallback } from "@/components/ui/avatar"
import { Button } from "@/components/ui/button"
import { ScrollArea } from "@/components/ui/scroll-area"
import { Sheet, SheetContent, SheetHeader, SheetTitle, SheetTrigger } from "@/components/ui/sheet"
import {
  SidebarGroup,
  SidebarGroupAction,
  SidebarGroupContent,
  SidebarGroupLabel,
  SidebarMenu,
  SidebarMenuBadge,
  SidebarMenuButton,
  SidebarMenuItem,
} from "@/components/ui/sidebar"

export interface ResourceItem {
  id: string
  title: string
  subtitle?: string
  status?: React.ReactNode
  icon?: React.ReactNode
}

interface ResourceSidebarProps {
  title: string
  items: ResourceItem[]
  selectedId?: string | null
  onSelect(id: string): void
  onCreate?: () => void
  footer?: React.ReactNode
}

function SidebarContent({ title, items, selectedId, onSelect, onCreate, footer }: ResourceSidebarProps) {
  return (
    <div className="flex h-full min-h-0 flex-col bg-sidebar text-sidebar-foreground">
      <ScrollArea className="min-h-0 flex-1">
        <SidebarGroup>
          <SidebarGroupLabel>{title}</SidebarGroupLabel>
          {onCreate && (
            <SidebarGroupAction onClick={onCreate} title={`New ${title.toLowerCase().replace(/s$/, "")}`}>
              <Plus />
              <span className="sr-only">New {title.toLowerCase().replace(/s$/, "")}</span>
            </SidebarGroupAction>
          )}
          <SidebarGroupContent>
            <SidebarMenu>
              {items.map((item) => (
                <SidebarMenuItem key={item.id}>
                  <SidebarMenuButton
                    size="lg"
                    isActive={selectedId === item.id}
                    onClick={() => onSelect(item.id)}
                    className="h-auto min-h-12"
                  >
                    <Avatar className="size-8 rounded-lg">
                      <AvatarFallback className="rounded-lg text-xs">
                        {item.icon || item.title.slice(0, 1).toUpperCase()}
                      </AvatarFallback>
                    </Avatar>
                    <span className="grid min-w-0 flex-1 text-left leading-tight">
                      <span className="truncate text-sm font-medium">{item.title}</span>
                      {item.subtitle && <span className="truncate text-xs text-muted-foreground">{item.subtitle}</span>}
                    </span>
                  </SidebarMenuButton>
                  {item.status && <SidebarMenuBadge>{item.status}</SidebarMenuBadge>}
                </SidebarMenuItem>
              ))}
            </SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
      </ScrollArea>
      {footer && <div className="shrink-0 border-t p-2">{footer}</div>}
    </div>
  )
}

export function ResourceSidebar(props: ResourceSidebarProps) {
  return (
    <>
      <aside data-resource-sidebar className="hidden h-full min-h-0 w-64 shrink-0 border-r lg:block">
        <SidebarContent {...props} />
      </aside>
      <div className="absolute top-3 left-11 z-20 lg:hidden">
        <Sheet>
          <SheetTrigger asChild>
            <Button variant="ghost" size="icon" aria-label={`Open ${props.title}`}><Menu data-icon="inline-start" /></Button>
          </SheetTrigger>
          <SheetContent side="left" className="w-72 gap-0 p-0">
            <SheetHeader className="sr-only"><SheetTitle>{props.title}</SheetTitle></SheetHeader>
            <SidebarContent {...props} />
          </SheetContent>
        </Sheet>
      </div>
    </>
  )
}
