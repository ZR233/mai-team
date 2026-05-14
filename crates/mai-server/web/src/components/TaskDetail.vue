<template>
  <template v-if="detail">
    <header class="agent-header task-header">
      <div class="agent-identity">
        <div class="large-avatar">{{ initial(detail.title) }}</div>
        <div class="agent-title">
          <div class="title-row">
            <h2>{{ detail.title }}</h2>
            <span class="status-pill" :class="statusTone(detail.status)">
              {{ formatStatus(detail.status) }}
            </span>
          </div>
          <p>
            <span>Plan {{ formatStatus(detail.plan?.status || detail.plan_status) }}</span>
            <span>/</span>
            <span>v{{ detail.plan?.version || detail.plan_version || 0 }}</span>
            <span>/</span>
            <span>{{ detail.agent_count }} agent{{ detail.agent_count === 1 ? '' : 's' }}</span>
          </p>
          <p v-if="detail.last_error" class="error-text">{{ detail.last_error }}</p>
          <p v-if="detail.final_report">{{ detail.final_report }}</p>
        </div>
      </div>
      <div class="agent-actions">
        <button class="ghost-button" type="button" @click="$emit('cancel', detail.id)">Cancel</button>
        <button class="danger-button" type="button" @click="$emit('delete', detail.id, detail.title)">Delete</button>
      </div>
    </header>

    <details class="task-plan-panel" :open="!planCollapsed">
      <summary class="task-plan-head">
        <div>
          <h3>{{ planTitle }}</h3>
          <p>{{ planMeta }}</p>
        </div>
      </summary>

      <div v-if="revisionFeedback" class="plan-feedback-banner">
        <strong>Revision requested:</strong>
        <p>{{ revisionFeedback }}</p>
      </div>

      <div v-if="planMarkdown" class="markdown-body task-plan-body" v-html="renderMarkdown(planMarkdown)"></div>
      <p v-else class="task-plan-empty">The planner has not saved a plan yet.</p>

      <details v-if="detail.plan_history && detail.plan_history.length > 0" class="plan-history">
        <summary>Plan History ({{ detail.plan_history.length }} version{{ detail.plan_history.length === 1 ? '' : 's' }})</summary>
        <div v-for="entry in detail.plan_history" :key="entry.version" class="plan-history-entry">
          <div class="plan-history-meta">
            <span>v{{ entry.version }}</span>
            <span v-if="entry.saved_at"> · saved {{ formatDate(entry.saved_at) }}</span>
          </div>
          <div v-if="entry.revision_feedback" class="plan-feedback-small">
            <strong>Feedback:</strong> {{ entry.revision_feedback }}
          </div>
        </div>
      </details>
    </details>

    <AgentDetail
      :detail="detail.selected_agent"
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
      :show-sessions="false"
      :show-composer="canCompose"
      :plan-approval-pending="canApprove"
      :approving-plan="approvingPlan"
      :plan-version="detail.plan?.version || 0"
      :input-enabled="inputEnabled"
      v-model:conversation-ref="conversationRef"
      @cancel="$emit('cancel-agent', $event)"
      @delete="(...args) => $emit('delete-agent', ...args)"
      @send="$emit('send', $event)"
      @stop="$emit('stop', $event)"
      @update-model="$emit('update-model', $event)"
      @open-providers="$emit('open-providers')"
      @update:draft="$emit('update:draft', $event)"
      @update:selected-skills="$emit('update:selectedSkills', $event)"
      @load-skills="$emit('load-skills')"
      @approve-plan="$emit('approve-plan')"
      @request-plan-revision="$emit('request-plan-revision', $event)"
    />
  </template>
</template>

<script setup>
import { computed, ref, watch } from 'vue'
import AgentDetail from './AgentDetail.vue'
import { formatDate, formatStatus, initial, statusTone } from '../utils/format'
import { renderMarkdown } from '../utils/markdown'

const props = defineProps({
  detail: { type: Object, default: null },
  events: { type: Array, required: true },
  draft: { type: String, default: '' },
  loading: { type: Boolean, default: false },
  sending: { type: Boolean, default: false },
  stopping: { type: Boolean, default: false },
  approvingPlan: { type: Boolean, default: false },
  updatingModel: { type: Boolean, default: false },
  providers: { type: Array, default: () => [] },
  skills: { type: Array, default: () => [] },
  selectedSkills: { type: Array, default: () => [] },
  skillsLoading: { type: Boolean, default: false },
  skillsError: { type: String, default: '' }
})

const conversationRef = defineModel('conversationRef', { default: null })

const emit = defineEmits([
  'approve-plan',
  'request-plan-revision',
  'cancel',
  'cancel-agent',
  'delete',
  'delete-agent',
  'send',
  'stop',
  'update:draft',
  'update:selectedSkills',
  'load-skills',
  'update-model',
  'open-providers'
])

const planCollapsed = ref(false)

const planTitle = computed(() => props.detail?.plan?.title || 'Task Plan')
const planMarkdown = computed(() => props.detail?.plan?.markdown || '')
const revisionFeedback = computed(() => props.detail?.plan?.revision_feedback || '')
const canApprove = computed(() => props.detail?.status === 'awaiting_approval' && props.detail?.plan?.status === 'ready')
const inputEnabled = computed(() => {
  const status = props.detail?.status
  return status === 'planning' || status === 'awaiting_approval'
      || status === 'completed' || status === 'failed'
})
const canCompose = computed(() => {
  const status = props.detail?.status
  const selectedRole = props.detail?.selected_agent?.role
  return selectedRole === 'planner'
      && (status === 'planning' || status === 'awaiting_approval'
       || status === 'completed' || status === 'failed')
})
const planMeta = computed(() => {
  const plan = props.detail?.plan
  if (!plan || plan.status === 'missing') return 'Waiting for planner output'
  const savedAt = plan.saved_at ? `saved ${formatDate(plan.saved_at)}` : 'saved'
  const approvedAt = plan.approved_at ? ` · approved ${formatDate(plan.approved_at)}` : ''
  return `v${plan.version} · ${formatStatus(plan.status)} · ${savedAt}${approvedAt}`
})

watch(
  () => props.detail?.plan?.status,
  (status) => {
    if (status === 'approved') planCollapsed.value = true
    else planCollapsed.value = false
  }
)
</script>
