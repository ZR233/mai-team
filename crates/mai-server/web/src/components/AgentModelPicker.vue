<template>
  <div ref="pickerRef" class="agent-model-picker">
    <button
      class="agent-model-chip"
      type="button"
      :disabled="controlsDisabled || !providers.length"
      :aria-expanded="open ? 'true' : 'false'"
      @click="toggleOpen"
    >
      <span class="model-dot"></span>
      <strong>{{ currentModelLabel }}</strong>
      <span>{{ currentProviderLabel }}<template v-if="currentReasoningLabel"> · {{ currentReasoningLabel }}</template></span>
      <span class="model-chevron">⌄</span>
    </button>

    <div v-if="open" class="agent-model-popover">
      <div class="model-popover-head">
        <div>
          <h3>Model for next turn</h3>
          <p>One control owns provider, model, depth, and save state.</p>
        </div>
        <span class="mini-pill" :class="dirty ? 'amber' : 'green'">{{ statusLabel }}</span>
      </div>

      <div class="model-picker-field">
        <span class="model-picker-label">Provider</span>
        <button class="model-picker-trigger" type="button" :disabled="controlsDisabled" @click="toggleDropdown('provider')">
          <span>{{ editorProvider?.name || editorProvider?.id || 'Select provider' }}</span>
          <span class="model-chevron">⌄</span>
        </button>
        <div v-if="openDropdown === 'provider'" class="model-picker-menu">
          <button
            v-for="provider in providers"
            :key="provider.id"
            class="model-picker-option"
            :class="{ active: provider.id === providerId }"
            type="button"
            @click="selectProvider(provider)"
          >
            <span>{{ provider.name || provider.id }}</span>
            <small>{{ provider.models?.length || 0 }} model{{ (provider.models?.length || 0) === 1 ? '' : 's' }}</small>
          </button>
        </div>
      </div>

      <div class="model-picker-field">
        <span class="model-picker-label">Model</span>
        <button class="model-picker-trigger" type="button" :disabled="controlsDisabled || !editorModels.length" @click="toggleDropdown('model')">
          <span>{{ editorModelLabel }}<template v-if="editorModelSummary"> · {{ editorModelSummary }}</template></span>
          <span class="model-chevron">⌄</span>
        </button>
        <div v-if="openDropdown === 'model'" class="model-picker-menu model-picker-menu-scroll">
          <label class="model-search-field">
            <span>⌕</span>
            <input v-model="modelQuery" type="search" placeholder="Search models..." @keydown.stop />
          </label>
          <div class="model-result-count">{{ filteredModels.length }} matching model{{ filteredModels.length === 1 ? '' : 's' }}</div>
          <button
            v-for="item in filteredModels"
            :key="item.id"
            class="model-picker-option model-option"
            :class="{ active: item.id === model }"
            type="button"
            @click="selectModel(item)"
          >
            <span>
              <strong>{{ item.name || item.id }}</strong>
              <small>{{ modelOptionSummary(item) || item.id }}</small>
            </span>
            <span v-if="item.id === model">✓</span>
          </button>
          <div v-if="!filteredModels.length" class="model-picker-empty">No matching models.</div>
        </div>
      </div>

      <div v-if="reasoningOptions.length" class="model-picker-field">
        <span class="model-picker-label">Thinking depth</span>
        <div class="model-depth-row">
          <button class="model-picker-trigger" type="button" :disabled="controlsDisabled" @click="toggleDropdown('reasoning')">
            <span>{{ editorReasoningLabel }}</span>
            <span class="model-chevron">⌄</span>
          </button>
          <small>Only shown when supported.</small>
        </div>
        <div v-if="openDropdown === 'reasoning'" class="model-picker-menu model-picker-menu-compact">
          <button
            v-for="option in reasoningOptions"
            :key="option.value"
            class="model-picker-option"
            :class="{ active: option.value === reasoningEffort }"
            type="button"
            @click="selectReasoning(option.value)"
          >
            <span>{{ option.label }}</span>
            <span v-if="option.value === reasoningEffort">✓</span>
          </button>
        </div>
      </div>

      <p v-if="error" class="dialog-error">{{ error }}</p>

      <div class="model-popover-actions">
        <span>{{ updating ? '保存中...' : '下一轮生效' }}</span>
        <button class="ghost-button" type="button" @click="$emit('open-providers')">Manage providers</button>
        <button class="primary-button" type="button" :disabled="!model || controlsDisabled" @click="apply">
          Apply
        </button>
      </div>
    </div>
  </div>
</template>

<script setup>
import { computed, onBeforeUnmount, ref, watch } from 'vue'
import { defaultReasoningEffort, reasoningLabel, reasoningOptionsFor } from '../utils/reasoning'
import { filterModels, modelOptionSummary, resolveModelSelection } from '../utils/modelPicker'

const props = defineProps({
  detail: { type: Object, required: true },
  providers: { type: Array, default: () => [] },
  reasoningEffort: { type: String, default: '' },
  disabled: { type: Boolean, default: false },
  updating: { type: Boolean, default: false }
})

const emit = defineEmits(['apply', 'open-providers'])

const pickerRef = ref(null)
const open = ref(false)
const openDropdown = ref('')
const providerId = ref('')
const model = ref('')
const reasoningEffort = ref('')
const modelQuery = ref('')
const error = ref('')

const currentProvider = computed(() => props.providers.find((provider) => provider.id === props.detail.provider_id))
const currentModel = computed(() => currentProvider.value?.models?.find((item) => item.id === props.detail.model))
const currentReasoningOptions = computed(() => reasoningOptionsFor(currentProvider.value, currentModel.value))
const currentModelLabel = computed(() => currentModel.value?.name || props.detail.model || 'Model')
const currentProviderLabel = computed(() => props.detail.provider_name || currentProvider.value?.name || props.detail.provider_id || 'Provider')
const currentReasoningLabel = computed(() => {
  const value = props.reasoningEffort || defaultReasoningEffort(currentProvider.value, currentModel.value)
  return currentReasoningOptions.value.length ? reasoningLabel(value) : ''
})

const editorProvider = computed(() => props.providers.find((provider) => provider.id === providerId.value))
const editorModels = computed(() => editorProvider.value?.models || [])
const editorModel = computed(() => editorModels.value.find((item) => item.id === model.value))
const editorModelLabel = computed(() => editorModel.value?.name || model.value || 'Select model')
const editorModelSummary = computed(() => modelOptionSummary(editorModel.value))
const filteredModels = computed(() => filterModels(editorModels.value, modelQuery.value))
const reasoningOptions = computed(() => reasoningOptionsFor(editorProvider.value, editorModel.value))
const editorReasoningLabel = computed(() => reasoningLabel(reasoningEffort.value) || 'Select depth')
const controlsDisabled = computed(() => props.disabled || props.updating)
const dirty = computed(() =>
  providerId.value !== (props.detail.provider_id || '')
    || model.value !== (props.detail.model || '')
    || reasoningEffort.value !== (props.reasoningEffort || '')
)
const statusLabel = computed(() => props.updating ? 'Saving...' : dirty.value ? 'Edited' : 'Saved')

watch(
  () => [props.detail.id, props.detail.provider_id, props.detail.model, props.reasoningEffort, props.providers],
  () => {
    if (!open.value) syncFromDetail()
  },
  { immediate: true }
)

watch(
  () => [props.detail.id, props.detail.selected_session_id],
  () => close()
)

watch(
  () => props.updating,
  (updating, wasUpdating) => {
    if (wasUpdating && !updating) close()
  }
)

watch(
  () => [props.detail.id, props.detail.selected_session_id],
  () => {
    close()
    syncFromDetail()
  }
)

function toggleOpen() {
  if (controlsDisabled.value || !props.providers.length) return
  if (open.value) {
    close()
    return
  }
  syncFromDetail()
  open.value = true
  document.addEventListener('pointerdown', onDocumentPointerDown)
}

function close() {
  open.value = false
  openDropdown.value = ''
  error.value = ''
  document.removeEventListener('pointerdown', onDocumentPointerDown)
}

function syncFromDetail() {
  const next = resolveModelSelection(
    props.providers,
    props.detail.provider_id,
    props.detail.model,
    props.reasoningEffort
  )
  providerId.value = next.providerId
  model.value = next.model
  reasoningEffort.value = next.reasoningEffort
  modelQuery.value = ''
  error.value = ''
}

function toggleDropdown(name) {
  if (controlsDisabled.value) return
  openDropdown.value = openDropdown.value === name ? '' : name
  if (name === 'model') modelQuery.value = ''
}

function selectProvider(provider) {
  const next = resolveModelSelection(props.providers, provider.id, '', '')
  providerId.value = next.providerId
  model.value = next.model
  reasoningEffort.value = next.reasoningEffort
  modelQuery.value = ''
  openDropdown.value = ''
}

function selectModel(item) {
  model.value = item.id
  const options = reasoningOptionsFor(editorProvider.value, item)
  reasoningEffort.value = options.some((option) => option.value === reasoningEffort.value)
    ? reasoningEffort.value
    : defaultReasoningEffort(editorProvider.value, item)
  openDropdown.value = ''
}

function selectReasoning(value) {
  reasoningEffort.value = value
  openDropdown.value = ''
}

function apply() {
  if (!providerId.value || !model.value) {
    error.value = 'Provider and model are required.'
    return
  }
  emit('apply', {
    agent_id: props.detail.id,
    provider_id: providerId.value,
    model: model.value,
    reasoning_effort: reasoningEffort.value
  })
}

function onDocumentPointerDown(event) {
  if (!pickerRef.value?.contains(event.target)) close()
}

onBeforeUnmount(() => {
  document.removeEventListener('pointerdown', onDocumentPointerDown)
})
</script>
