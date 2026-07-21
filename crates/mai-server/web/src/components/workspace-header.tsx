import { Fragment } from "react"
import { Link } from "react-router-dom"

import {
  Breadcrumb,
  BreadcrumbItem,
  BreadcrumbLink,
  BreadcrumbList,
  BreadcrumbSeparator,
} from "@/components/ui/breadcrumb"
import { Separator } from "@/components/ui/separator"
import { SidebarTrigger } from "@/components/ui/sidebar"

export interface WorkspaceCrumb {
  label: string
  href?: string
}

export function WorkspaceHeader({ crumbs, actions, resourceTrigger }: {
  crumbs: WorkspaceCrumb[]
  actions?: React.ReactNode
  resourceTrigger?: React.ReactNode
}) {
  return (
    <header data-workspace-header className="flex h-14 shrink-0 items-center gap-2 border-b bg-background px-3 md:px-4">
      <SidebarTrigger />
      {resourceTrigger}
      <Separator orientation="vertical" className="mr-1 data-[orientation=vertical]:h-4" />
      <Breadcrumb className="min-w-0 flex-1">
        <BreadcrumbList className="flex-nowrap">
          {crumbs.map((crumb, index) => {
            const last = index === crumbs.length - 1
            return (
              <Fragment key={`${crumb.label}:${index}`}>
                <BreadcrumbItem className={last ? "min-w-0" : "hidden sm:inline-flex"}>
                  {last
                    ? <h1 aria-current="page" className="truncate font-medium text-foreground">{crumb.label}</h1>
                    : crumb.href
                      ? <BreadcrumbLink asChild><Link to={crumb.href}>{crumb.label}</Link></BreadcrumbLink>
                      : <span>{crumb.label}</span>}
                </BreadcrumbItem>
                {!last && <BreadcrumbSeparator className="hidden sm:list-item" />}
              </Fragment>
            )
          })}
        </BreadcrumbList>
      </Breadcrumb>
      {actions && <div className="flex shrink-0 items-center gap-2">{actions}</div>}
    </header>
  )
}
