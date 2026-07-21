import { Bot, Github, KeyRound, Search, ServerCog, Sparkles } from "lucide-react"
import { Navigate, useNavigate, useParams } from "react-router-dom"

import { ResourceSidebar } from "@/components/resource-sidebar"
import { WorkspaceHeader } from "@/components/workspace-header"

import { GitAccountsSection } from "./sections/git-accounts-section"
import { GithubAppSection } from "./sections/github-app-section"
import { McpSection } from "./sections/mcp-section"
import { RolesSection } from "./sections/roles-section"
import { SkillsSection } from "./sections/skills-section"
import { WebSearchSection } from "./sections/web-search-section"

const sections = [
  { id: "roles", title: "Role Models", detail: "Planner · Explorer · Executor · Reviewer", icon: Bot },
  { id: "skills", title: "Skills", detail: "Discovery and activation", icon: Sparkles },
  { id: "git-accounts", title: "Git Accounts", detail: "Repository credentials", icon: KeyRound },
  { id: "github-app", title: "GitHub App", detail: "Relay and installations", icon: Github },
  { id: "web-search", title: "Web Search", detail: "Provider capability planning", icon: Search },
  { id: "mcp", title: "MCP Servers", detail: "Built-in and custom tools", icon: ServerCog },
] as const

type SettingsSection = typeof sections[number]["id"]

export default function SettingsPage() {
  const { section } = useParams()
  const navigate = useNavigate()
  const active = (section || "roles") as SettingsSection
  if (!sections.some((candidate) => candidate.id === active)) return <Navigate to="/settings/roles" replace />
  const activeSection = sections.find((candidate) => candidate.id === active) ?? sections[0]

  return (
    <div className="relative flex h-full min-h-0">
      <ResourceSidebar title="Settings" items={sections.map(({ id, title, detail, icon: Icon }) => ({ id, title, subtitle: detail, icon: <Icon className="size-4" /> }))} selectedId={active} onSelect={(id) => navigate(`/settings/${id}`)} />
      <section className="relative flex min-h-0 min-w-0 flex-1 flex-col bg-background">
        <WorkspaceHeader crumbs={[{ label: "Settings", href: "/settings" }, { label: activeSection.title }]} />
        {active === "roles" && <RolesSection />}
        {active === "skills" && <SkillsSection />}
        {active === "git-accounts" && <GitAccountsSection />}
        {active === "github-app" && <GithubAppSection />}
        {active === "web-search" && <WebSearchSection />}
        {active === "mcp" && <McpSection />}
      </section>
    </div>
  )
}
