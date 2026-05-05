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

    <section class="task-plan-panel">
      <div class="task-plan-head">
        <div>
          <h3>{{ planTitle }}</h3>
          <p>{{ planMeta }}</p>
        </div>
        <button
          v-if="canApprove"
          class="primary-button"
          type="button"
          :disabled="approvingPlan"
          @click="$emit('approve-plan')"
        >
          <span v-if="approvingPlan" class="spinner-sm"></span>
          <template v-else>Approve Plan</template>
        </button>
      </div>
      <div v-if="planMarkdown" class="markdown-body task-plan-body" v-html="renderMarkdown(planMarkdown)"></div>
      <p v-else class="task-plan-empty">The planner has not saved a plan yet.</p>
    </section>

    <AgentDetail
      :detail="detail.selected_agent"
      :events="events"
      :draft="draft"
      :loading="loading"
      :sending="sending"
      :providers="providers"
      :updating-model="updatingModel"
      :show-sessions="false"
      :show-composer="canCompose"
      v-model:conversation-ref="conversationRef"
      @cancel="$emit('cancel-agent', $event)"
      @delete="(...args) => $emit('delete-agent', ...args)"
      @send="$emit('send', $event)"
      @update-model="$emit('update-model', $event)"
      @update:draft="$emit('update:draft', $event)"
    />
  </template>
</template>

<script setup>
import { computed } from 'vue'
import AgentDetail from './AgentDetail.vue'
import { formatDate, formatStatus, initial, statusTone } from '../utils/format'
import { renderMarkdown } from '../utils/markdown'

const props = defineProps({
  detail: { type: Object, default: null },
  events: { type: Array, required: true },
  draft: { type: String, default: '' },
  loading: { type: Boolean, default: false },
  sending: { type: Boolean, default: false },
  approvingPlan: { type: Boolean, default: false },
  updatingModel: { type: Boolean, default: false },
  providers: { type: Array, default: () => [] }
})

const conversationRef = defineModel('conversationRef', { default: null })

defineEmits([
  'approve-plan',
  'cancel',
  'cancel-agent',
  'delete',
  'delete-agent',
  'send',
  'update:draft',
  'update-model'
])

const planTitle = computed(() => props.detail?.plan?.title || 'Task Plan')
const planMarkdown = computed(() => props.detail?.plan?.markdown || '')
const canApprove = computed(() => props.detail?.status === 'awaiting_approval' && props.detail?.plan?.status === 'ready')
const canCompose = computed(() => {
  const status = props.detail?.status
  const selectedRole = props.detail?.selected_agent?.role
  return selectedRole === 'planner' && (status === 'planning' || status === 'awaiting_approval')
})
const planMeta = computed(() => {
  const plan = props.detail?.plan
  if (!plan || plan.status === 'missing') return 'Waiting for planner output'
  const savedAt = plan.saved_at ? `saved ${formatDate(plan.saved_at)}` : 'saved'
  const approvedAt = plan.approved_at ? ` · approved ${formatDate(plan.approved_at)}` : ''
  return `v${plan.version} · ${formatStatus(plan.status)} · ${savedAt}${approvedAt}`
})
</script>
