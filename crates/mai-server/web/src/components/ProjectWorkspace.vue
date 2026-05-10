<template>
  <section class="projects-stage">
    <aside class="agent-rail project-rail">
      <div class="rail-head">
        <div>
          <h2>Projects</h2>
          <p>{{ projects.length }} project{{ projects.length === 1 ? '' : 's' }}</p>
        </div>
        <button class="small-button" type="button" @click="$emit('create')">New Project</button>
      </div>
      <div class="agent-list">
        <button
          v-for="project in projects"
          :key="project.id"
          class="agent-row project-row"
          :class="{ active: selectedProjectId === project.id }"
          @click="$emit('select-project', project.id)"
        >
          <span class="avatar">{{ initial(project.name) }}</span>
          <span class="agent-row-main">
            <span class="agent-name">
              <span class="agent-name-text">{{ project.name }}</span>
            </span>
            <span class="agent-meta">{{ repositoryName(project) }} · {{ projectStageLabel(project) }}</span>
          </span>
          <span class="status-dot" :class="statusTone(project.status)" :title="projectStageLabel(project)" />
        </button>
        <div v-if="!projects.length" class="empty-rail">
          <strong>No projects yet</strong>
          <span>Create a project from a GitHub repository.</span>
        </div>
      </div>
    </aside>

    <aside class="settings-sidebar project-subnav" aria-label="Project sections">
      <button
        v-for="item in navItems"
        :key="item.id"
        type="button"
        class="settings-nav-item"
        :class="{ active: activeSection === item.id }"
        :disabled="!detail"
        @click="activeSection = item.id"
      >
        <span class="settings-nav-icon">{{ item.icon }}</span>
        <span class="settings-nav-copy">
          <strong>{{ item.label }}</strong>
          <small>{{ item.meta }}</small>
        </span>
      </button>
    </aside>

    <section class="project-content">
      <div v-if="!detail" class="empty-stage">
        <div class="empty-mark">P</div>
        <h2>No project selected</h2>
        <p>Create a project or choose one from the left rail.</p>
        <button class="primary-button" type="button" @click="$emit('create')">New Project</button>
      </div>

      <div v-else-if="isProjectSettingUp" class="project-panel project-progress-panel">
        <header class="settings-section-header">
          <div>
            <h2>{{ projectStageTitle }}</h2>
            <p>{{ repositoryName(detail) }}</p>
          </div>
          <span class="section-status" :class="statusTone(detail.status)">{{ projectStageLabel(detail) }}</span>
        </header>
        <div class="project-progress-bar" :class="{ failed: isProjectFailed }">
          <span></span>
        </div>
        <div class="settings-summary">
          <div
            v-for="step in projectSetupSteps"
            :key="step.id"
            class="settings-summary-item"
            :class="{ ready: step.done, danger: step.failed }"
          >
            <span>{{ step.label }}</span>
            <strong>{{ step.value }}</strong>
            <small>{{ step.help }}</small>
          </div>
        </div>
        <div class="project-facts">
          <div class="project-fact">
            <span>Repository</span>
            <strong>{{ detail.repository_full_name || repositoryName(detail) }}</strong>
          </div>
          <div class="project-fact">
            <span>Branch</span>
            <strong>{{ detail.branch || 'Default branch' }}</strong>
          </div>
          <div class="project-fact span-2">
            <span>Docker Image</span>
            <strong>{{ detail.docker_image || 'Default image' }}</strong>
          </div>
          <div class="project-fact span-2" :class="{ danger: detail.last_error }">
            <span>Status</span>
            <strong>{{ detail.last_error || projectProgressMessage }}</strong>
          </div>
        </div>
      </div>

      <template v-else-if="activeSection === 'planner'">
        <AgentDetail
          :detail="detail.maintainer_agent"
          :events="events"
          :draft="draft"
          :loading="loading"
          :sending="sending"
          :stopping="stopping"
          :providers="providers"
          :skills="skills"
          :selected-skills="selectedSkills"
          :skills-loading="skillsLoading"
          :skills-error="skillsError"
          :updating-model="updatingModel"
          :show-sessions="true"
          :show-composer="true"
          v-model:conversation-ref="conversationRef"
          @cancel="$emit('cancel-agent', $event)"
          @delete="(...args) => $emit('delete-agent', ...args)"
          @send="$emit('send', $event)"
          @stop="$emit('stop', $event)"
          @update-model="$emit('update-model', $event)"
          @update:draft="$emit('update:draft', $event)"
          @update:selected-skills="$emit('update:selectedSkills', $event)"
          @load-skills="$emit('load-skills')"
          @create-session="$emit('create-session')"
          @select-session="$emit('select-session', $event)"
        />
      </template>

      <div v-else-if="activeSection === 'review'" class="project-panel">
        <header class="settings-section-header">
          <div>
            <h2>Review Status</h2>
            <p>{{ repositoryName(detail) }}</p>
          </div>
          <span class="section-status" :class="statusTone(detail.status)">{{ formatStatus(detail.status) }}</span>
        </header>
        <div class="review-settings">
          <div class="review-toggle-row">
            <div>
              <strong>Automatic PR Review</strong>
              <span>Runs one ephemeral reviewer agent per cycle with a project reviewer workspace.</span>
            </div>
            <button
              class="small-button"
              type="button"
              :class="{ secondary: detail.auto_review_enabled }"
              :disabled="!projectReady"
              @click="toggleAutoReview"
            >
              {{ detail.auto_review_enabled ? 'Disable' : 'Enable' }}
            </button>
          </div>
          <label class="review-prompt-field">
            <span>Reviewer extra prompt</span>
            <textarea
              v-model="reviewerExtraPromptDraft"
              rows="5"
              placeholder="Focus areas, repository conventions, or review policy for this project"
            ></textarea>
          </label>
          <div class="review-settings-actions">
            <small>{{ projectReady ? reviewSettingsHint : 'Project review can be enabled after setup completes.' }}</small>
            <button
              class="ghost-button"
              type="button"
              :disabled="!reviewPromptDirty"
              @click="saveReviewerPrompt"
            >
              Save Prompt
            </button>
          </div>
        </div>
        <div class="settings-summary">
          <div class="settings-summary-item">
            <span>Auto Review</span>
            <strong>{{ detail.auto_review_enabled ? 'Enabled' : 'Disabled' }}</strong>
            <small>{{ formatStatus(detail.review_status || 'disabled') }}</small>
          </div>
          <div class="settings-summary-item" :class="{ danger: detail.review_last_error }">
            <span>Outcome</span>
            <strong>{{ formatStatus(detail.last_review_outcome || 'none') }}</strong>
            <small>{{ detail.review_last_error || reviewLastFinishedLabel }}</small>
          </div>
          <div class="settings-summary-item">
            <span>Next Run</span>
            <strong>{{ nextReviewLabel }}</strong>
            <small>{{ currentReviewerLabel }}</small>
          </div>
        </div>
      </div>

      <div v-else-if="activeSection === 'repository'" class="project-panel">
        <header class="settings-section-header">
          <div>
            <h2>Repository</h2>
            <p>{{ repositoryName(detail) }}</p>
          </div>
          <span class="section-status">Git Account</span>
        </header>
        <div class="project-facts">
          <div class="project-fact">
            <span>Owner / Repo</span>
            <strong>{{ detail.repository_full_name || repositoryName(detail) }}</strong>
          </div>
          <div class="project-fact">
            <span>Branch</span>
            <strong>{{ detail.branch || 'Default branch' }}</strong>
          </div>
          <div class="project-fact">
            <span>Git Account</span>
            <strong>{{ detail.installation_account || 'Unknown' }}</strong>
          </div>
          <div class="project-fact">
            <span>Account ID</span>
            <strong>{{ detail.git_account_id || 'Unknown' }}</strong>
          </div>
          <div class="project-fact span-2">
            <span>Docker Image</span>
            <strong>{{ detail.docker_image || 'Default image' }}</strong>
          </div>
          <div class="project-fact">
            <span>Auth Mode</span>
            <strong>Git account token</strong>
          </div>
          <div class="project-fact">
            <span>Agent Tools</span>
            <strong>Git MCP + GitHub MCP</strong>
          </div>
          <div class="project-fact">
            <span>MCP Status</span>
            <strong>{{ formatStatus(detail.mcp_status || detail.mcp_auth_status || 'unknown') }}</strong>
          </div>
          <div class="project-fact">
            <span>Auth Status</span>
            <strong>{{ formatStatus(detail.auth_status || 'unknown') }}</strong>
          </div>
          <div class="project-fact" :class="{ danger: detail.last_error }">
            <span>Clone Status</span>
            <strong>{{ formatStatus(detail.clone_status) }}</strong>
          </div>
          <div class="project-fact" :class="{ danger: detail.last_error }">
            <span>Last Error</span>
            <strong>{{ detail.last_error || 'None' }}</strong>
          </div>
        </div>
      </div>

      <div v-else-if="activeSection === 'skills'" class="project-panel">
        <header class="settings-section-header">
          <div>
            <h2>Skills</h2>
            <p>{{ repositoryName(detail) }}</p>
          </div>
          <button
            class="ghost-button"
            type="button"
            :disabled="projectSkillsBusy || !projectReady"
            @click="$emit('detect-project-skills')"
          >
            <span v-if="projectSkillsState.refreshing" class="spinner-sm"></span>
            <template v-else>Refresh</template>
          </button>
        </header>
        <div class="settings-summary">
          <div class="settings-summary-item" :class="{ ready: projectSkillCount }">
            <span>Detected</span>
            <strong>{{ projectSkillCount }}</strong>
            <small>Project-scoped skills</small>
          </div>
          <div class="settings-summary-item">
            <span>Roots</span>
            <strong>{{ projectRootCount }}</strong>
            <small>.claude, .agents, and skills</small>
          </div>
          <div class="settings-summary-item" :class="{ danger: projectSkillIssueCount }">
            <span>Parse Errors</span>
            <strong>{{ projectSkillIssueCount }}</strong>
            <small>{{ projectSkillsState.error || 'No runtime detection error' }}</small>
          </div>
        </div>
        <div class="skills-root-list">
          <span v-for="root in projectSkillsState.roots" :key="root" class="skill-root">{{ root }}</span>
          <span v-if="!projectSkillsState.roots?.length" class="skill-root muted">No project skill roots detected</span>
        </div>
        <div v-if="projectSkillsState.error" class="skill-error-row">
          <strong>Detection failed</strong>
          <span>{{ projectSkillsState.error }}</span>
        </div>
        <div v-if="projectSkillsState.errors?.length" class="skill-errors">
          <div v-for="item in projectSkillsState.errors" :key="item.path" class="skill-error-row">
            <strong>{{ item.path }}</strong>
            <span>{{ item.message }}</span>
          </div>
        </div>
        <div class="skills-config-list">
          <div
            v-for="skill in projectSkillsState.skills"
            :key="skill.path"
            class="skill-config-row"
          >
            <span class="skill-toggle on">On</span>
            <span class="skill-config-main">
              <strong>{{ skillDisplayName(skill) }}</strong>
              <small>{{ skill.description || skill.short_description }}</small>
              <code>{{ skill.source_path || skill.path }}</code>
            </span>
            <span class="mini-pill">{{ skill.scope }}</span>
          </div>
          <div v-if="projectSkillsEmpty" class="quiet-empty skills-empty">
            <strong>No project skills found</strong>
            <span>Checked `.claude/skills`, `.agents/skills`, and `skills` under `/workspace/repo`.</span>
          </div>
          <div v-if="projectSkillsState.loading" class="loading-center">
            <span class="spinner"></span>
            <span>Loading project skills...</span>
          </div>
        </div>
      </div>

      <div v-else class="project-panel">
        <header class="settings-section-header">
          <div>
            <h2>Agents</h2>
            <p>{{ projectAgents.length }} agent{{ projectAgents.length === 1 ? '' : 's' }}</p>
          </div>
        </header>
        <div class="project-agent-list">
          <button
            v-for="agent in projectAgents"
            :key="agent.id"
            type="button"
            class="agent-row project-agent-row"
            :class="{ active: detail.maintainer_agent?.id === agent.id }"
            @click="$emit('select-agent', agent.id)"
          >
            <span class="avatar">{{ roleInitial(agent.role) }}</span>
            <span class="agent-row-main">
              <span class="agent-name">
                <span class="agent-name-text">{{ agent.name || agent.id }}</span>
              </span>
              <span class="agent-meta">{{ formatStatus(agent.role) }} · {{ agent.model || 'model unset' }}</span>
            </span>
            <span class="status-dot" :class="statusTone(agent.status)" :title="formatStatus(agent.status)" />
          </button>
        </div>
      </div>
    </section>
  </section>
</template>

<script setup>
import { computed, ref, watch } from 'vue'
import AgentDetail from './AgentDetail.vue'
import { formatStatus, initial, statusTone } from '../utils/format'

const props = defineProps({
  projects: { type: Array, required: true },
  detail: { type: Object, default: null },
  selectedProjectId: { type: String, default: null },
  events: { type: Array, required: true },
  draft: { type: String, default: '' },
  loading: { type: Boolean, default: false },
  sending: { type: Boolean, default: false },
  stopping: { type: Boolean, default: false },
  updatingModel: { type: Boolean, default: false },
  providers: { type: Array, default: () => [] },
  skills: { type: Array, default: () => [] },
  selectedSkills: { type: Array, default: () => [] },
  skillsLoading: { type: Boolean, default: false },
  skillsError: { type: String, default: '' },
  projectSkillsState: { type: Object, default: () => ({}) }
})

const conversationRef = defineModel('conversationRef', { default: null })

const emit = defineEmits([
  'create',
  'select-project',
  'select-agent',
  'cancel-project',
  'cancel-agent',
  'delete-project',
  'delete-agent',
  'send',
  'stop',
  'update-model',
  'update:draft',
  'update:selectedSkills',
  'load-skills',
  'detect-project-skills',
  'update-review-settings',
  'create-session',
  'select-session'
])

const activeSection = ref('planner')
const navItems = [
  { id: 'planner', label: 'Planner Chat', meta: 'Project maintainer', icon: 'P' },
  { id: 'review', label: 'Review Status', meta: 'Project health', icon: 'R' },
  { id: 'repository', label: 'Repository', meta: 'GitHub and workspace', icon: 'G' },
  { id: 'skills', label: 'Skills', meta: 'Project scope', icon: 'S' },
  { id: 'agents', label: 'Agents', meta: 'Project agents', icon: 'A' }
]

const projectAgents = computed(() => {
  const agents = props.detail?.agents?.length ? props.detail.agents : [props.detail?.maintainer_agent].filter(Boolean)
  return agents
})

const isProjectFailed = computed(() => props.detail?.status === 'failed' || props.detail?.clone_status === 'failed')
const isProjectSettingUp = computed(() => {
  if (!props.detail) return false
  if (isProjectFailed.value) return true
  return props.detail.status === 'creating'
    || props.detail.clone_status === 'pending'
    || props.detail.clone_status === 'cloning'
})
const projectStageTitle = computed(() => (isProjectFailed.value ? 'Project setup failed' : 'Setting up project'))
const projectReady = computed(() => props.detail?.status === 'ready' || props.detail?.clone_status === 'ready')
const projectSkillsBusy = computed(() => props.projectSkillsState?.loading || props.projectSkillsState?.refreshing)
const projectSkillCount = computed(() => props.projectSkillsState?.skills?.length || 0)
const projectRootCount = computed(() => props.projectSkillsState?.roots?.length || 0)
const projectSkillIssueCount = computed(() => props.projectSkillsState?.errors?.length || 0)
const projectSkillsEmpty = computed(() => (
  !projectSkillsBusy.value
    && props.projectSkillsState?.loaded
    && !props.projectSkillsState?.error
    && !projectSkillCount.value
))
const reviewerExtraPromptDraft = ref('')
const reviewPromptDirty = computed(() => reviewerExtraPromptDraft.value !== (props.detail?.reviewer_extra_prompt || ''))
const reviewSettingsHint = computed(() => (
  props.detail?.auto_review_enabled
    ? 'The scheduler keeps the reviewer workspace warm and polls for eligible PRs.'
    : 'Enable to let Mai periodically review eligible open pull requests.'
))
const nextReviewLabel = computed(() => {
  if (!props.detail?.auto_review_enabled) return 'Not scheduled'
  return props.detail?.next_review_at ? formatDateTime(props.detail.next_review_at) : 'As soon as possible'
})
const reviewLastFinishedLabel = computed(() => (
  props.detail?.last_review_finished_at
    ? `Finished ${formatDateTime(props.detail.last_review_finished_at)}`
    : 'No review cycle completed yet'
))
const currentReviewerLabel = computed(() => (
  props.detail?.current_reviewer_agent_id
    ? `Reviewer ${props.detail.current_reviewer_agent_id}`
    : 'No reviewer running'
))
const projectProgressMessage = computed(() => {
  if (!props.detail) return ''
  if (isProjectFailed.value) return 'Project setup failed.'
  if (props.detail.clone_status === 'cloning') return 'Cloning the repository into the project workspace.'
  if (props.detail.maintainer_agent?.status === 'starting_container') return 'Starting the maintainer workspace container.'
  return 'Project accepted. Preparing the maintainer workspace.'
})
const projectSetupSteps = computed(() => {
  const detail = props.detail || {}
  const cloneReady = detail.clone_status === 'ready'
  const cloning = detail.clone_status === 'cloning'
  const failed = isProjectFailed.value
  return [
    {
      id: 'accepted',
      label: 'Accepted',
      value: 'Project saved',
      help: 'The server accepted this project.',
      done: true,
      failed: false
    },
    {
      id: 'workspace',
      label: 'Workspace',
      value: failed ? 'Failed' : (cloning || cloneReady ? 'Started' : 'Starting'),
      help: detail.maintainer_agent?.status ? formatStatus(detail.maintainer_agent.status) : 'Waiting for maintainer container',
      done: cloning || cloneReady,
      failed
    },
    {
      id: 'clone',
      label: 'Clone',
      value: failed ? 'Failed' : (cloneReady ? 'Ready' : (cloning ? 'Cloning' : 'Pending')),
      help: failed ? (detail.last_error || 'Clone did not complete') : 'Repository path: /workspace/repo',
      done: cloneReady,
      failed
    }
  ]
})

watch(
  () => props.selectedProjectId,
  () => {
    activeSection.value = 'planner'
  }
)

watch(
  () => props.detail?.reviewer_extra_prompt,
  (value) => {
    reviewerExtraPromptDraft.value = value || ''
  },
  { immediate: true }
)

function repositoryName(project) {
  return [project?.owner, project?.repo].filter(Boolean).join('/') || 'Repository pending'
}

function projectStageLabel(project) {
  if (!project) return 'Pending'
  if (project.status === 'failed' || project.clone_status === 'failed') return 'Failed'
  if (project.status === 'ready' || project.clone_status === 'ready') return 'Ready'
  if (project.clone_status === 'cloning') return 'Cloning repository'
  if (project.clone_status === 'pending') return 'Starting workspace'
  return formatStatus(project.status)
}

function roleInitial(role) {
  const value = String(role || '').toLowerCase()
  if (value === 'maintainer') return 'M'
  if (value === 'planner') return 'P'
  if (value === 'reviewer') return 'R'
  if (value === 'executor') return 'E'
  return 'A'
}

function skillDisplayName(skill) {
  return skill?.interface?.display_name || skill?.name || 'Skill'
}

function toggleAutoReview() {
  if (!props.detail?.id) return
  emitReviewSettings({
    auto_review_enabled: !props.detail.auto_review_enabled,
    reviewer_extra_prompt: reviewerExtraPromptDraft.value
  })
}

function saveReviewerPrompt() {
  if (!props.detail?.id) return
  emitReviewSettings({
    reviewer_extra_prompt: reviewerExtraPromptDraft.value
  })
}

function emitReviewSettings(patch) {
  emit('update-review-settings', {
    projectId: props.detail.id,
    patch
  })
}

function formatDateTime(value) {
  if (!value) return 'Never'
  const date = new Date(value)
  if (Number.isNaN(date.getTime())) return value
  return date.toLocaleString()
}
</script>
