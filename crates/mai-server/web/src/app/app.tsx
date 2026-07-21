import { lazy, Suspense } from "react"
import { Navigate, Route, Routes } from "react-router-dom"

import { LoadingState } from "@/components/page-state"
import { AppShell } from "@/app/app-shell"

const ChatPage = lazy(() => import("@/features/chat/chat-page"))
const ProjectsPage = lazy(() => import("@/features/projects/projects-page"))
const ProvidersPage = lazy(() => import("@/features/providers/providers-page"))
const SettingsPage = lazy(() => import("@/features/settings/settings-page"))
const TasksPage = lazy(() => import("@/features/tasks/tasks-page"))

export function App() {
  return (
    <Suspense fallback={<LoadingState rows={6} />}>
      <Routes>
        <Route element={<AppShell />}>
          <Route index element={<Navigate to="/chat" replace />} />
          <Route path="chat/:environmentId?" element={<ChatPage />} />
          <Route path="tasks/:taskId?" element={<TasksPage />} />
          <Route path="projects/:projectId?" element={<ProjectsPage />} />
          <Route path="providers" element={<ProvidersPage />} />
          <Route path="settings/:section?" element={<SettingsPage />} />
        </Route>
        <Route path="*" element={<Navigate to="/chat" replace />} />
      </Routes>
    </Suspense>
  )
}
