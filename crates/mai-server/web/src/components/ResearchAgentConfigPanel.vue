<template>
  <section class="settings-stage">
    <div class="stage-title">
      <div>
        <h2>Task Role Settings</h2>
        <p>Assign model profiles to planner, explorer, executor, and reviewer task-agent roles.</p>
      </div>
    </div>

    <div v-if="!providers.length" class="empty-stage providers-empty">
      <div class="empty-mark">P</div>
      <h2>No providers configured</h2>
      <p>Add a provider before configuring task-agent roles.</p>
      <button class="primary-button" @click="$emit('open-providers')">Open Providers</button>
    </div>

    <form v-else class="settings-console" @submit.prevent="save">
      <div class="settings-summary">
        <div class="settings-summary-item">
          <span>Provider seed</span>
          <strong>{{ providerSeedLabel }}</strong>
          <small>Empty roles are prefilled from the first provider.</small>
        </div>
        <div class="settings-summary-item" :class="canSave ? 'ready' : 'danger'">
          <span>Configuration</span>
          <strong>{{ canSave ? 'Ready' : 'Incomplete' }}</strong>
          <small>{{ configuredCount }} of {{ ROLE_DEFINITIONS.length }} roles saved or staged.</small>
        </div>
        <div class="settings-summary-item">
          <span>Spawn default</span>
          <strong>Executor</strong>
          <small>Task workflows use executor after plan approval.</small>
        </div>
      </div>

      <div class="role-config-list">
        <article
          v-for="role in ROLE_DEFINITIONS"
          :key="role.key"
          class="role-config-row"
          :class="`role-${role.key}`"
        >
          <div class="role-card-head">
            <div class="role-avatar">{{ role.initial }}</div>
            <div>
              <h3>{{ role.title }}</h3>
              <p>{{ role.description }}</p>
            </div>
            <span class="role-status" :class="{ staged: !savedPreference(role.key) }">
              {{ savedPreference(role.key) ? 'Configured' : 'Prefilled' }}
            </span>
          </div>

          <ModelSelector
            v-model:provider-id="forms[role.key].provider_id"
            v-model:model="forms[role.key].model"
            v-model:reasoning-effort="forms[role.key].reasoning_effort"
            :providers="providers"
            :disabled="saving"
            class="role-model-selector"
          />

          <div class="effective-model-line role-effective-line">
            <span>Effective</span>
            <strong>{{ effectiveLabel(role.key) }}</strong>
          </div>
        </article>
      </div>

      <div class="settings-actions">
        <div class="settings-actions-left">
          <button class="ghost-button" type="button" :disabled="saving" @click="resetFromState">Reset</button>
          <button class="ghost-button" type="button" :disabled="loading" @click="$emit('reload')">Reload</button>
        </div>
        <div class="settings-action-errors">
          <p v-if="state.validation_error" class="dialog-error">{{ state.validation_error }}</p>
          <p v-if="error" class="dialog-error">{{ error }}</p>
        </div>
        <button class="primary-button" type="submit" :disabled="saving || !canSave">
          <span v-if="saving" class="spinner-sm"></span>
          <template v-else>Save Config</template>
        </button>
      </div>
    </form>
  </section>
</template>

<script setup>
import { computed, reactive, ref, watch } from 'vue'
import ModelSelector from './ModelSelector.vue'
import { defaultReasoningEffort, reasoningLabel } from '../utils/reasoning'

const ROLE_DEFINITIONS = [
  {
    key: 'planner',
    title: 'Planner',
    initial: 'P',
    description: 'Decomposes tasks, plans steps, and calls out dependencies.'
  },
  {
    key: 'explorer',
    title: 'Explorer',
    initial: 'X',
    description: 'Explores code, docs, and web context before implementation.'
  },
  {
    key: 'executor',
    title: 'Executor',
    initial: 'E',
    description: 'Writes code, runs commands, and completes implementation work.'
  },
  {
    key: 'reviewer',
    title: 'Reviewer',
    initial: 'R',
    description: 'Reviews changes, finds regressions, and highlights test gaps.'
  }
]

const props = defineProps({
  providers: { type: Array, default: () => [] },
  state: { type: Object, required: true },
  loading: { type: Boolean, default: false },
  saving: { type: Boolean, default: false }
})

const emit = defineEmits(['save', 'reload', 'open-providers'])

const error = ref('')
const forms = reactive({
  planner: emptyPreference(),
  explorer: emptyPreference(),
  executor: emptyPreference(),
  reviewer: emptyPreference()
})

const providerSeedLabel = computed(() => {
  const provider = props.providers[0]
  if (!provider) return 'No provider'
  const model = provider.default_model || provider.models?.[0]?.id || 'No model'
  return `${provider.name || provider.id} / ${model}`
})

const configuredCount = computed(() =>
  ROLE_DEFINITIONS.filter((role) => savedPreference(role.key)).length
)

const canSave = computed(() =>
  ROLE_DEFINITIONS.every((role) => forms[role.key].provider_id && forms[role.key].model)
)

watch(
  () => [
    props.state.planner,
    props.state.explorer,
    props.state.executor,
    props.state.reviewer,
    props.providers
  ],
  resetFromState,
  { immediate: true }
)

function emptyPreference() {
  return {
    provider_id: '',
    model: '',
    reasoning_effort: ''
  }
}

function savedPreference(role) {
  return props.state?.[role] || null
}

function effectivePreference(role) {
  return props.state?.[`effective_${role}`] || null
}

function resetFromState() {
  for (const role of ROLE_DEFINITIONS) {
    const configured = savedPreference(role.key)
    const fallback = defaultPreference(configured)
    forms[role.key].provider_id = configured?.provider_id || fallback.provider_id
    forms[role.key].model = configured?.model || fallback.model
    forms[role.key].reasoning_effort = configured?.reasoning_effort || fallback.reasoning_effort
  }
  error.value = ''
}

function defaultPreference(configured) {
  const provider = configured
    ? props.providers.find((item) => item.id === configured.provider_id) || props.providers[0]
    : props.providers[0]
  const modelId = configured?.model || provider?.default_model || provider?.models?.[0]?.id || ''
  const model = provider?.models?.find((item) => item.id === modelId)
  return {
    provider_id: provider?.id || '',
    model: modelId,
    reasoning_effort: defaultReasoningEffort(provider, model)
  }
}

function effectiveLabel(role) {
  const staged = forms[role]
  const effective = effectivePreference(role)
  if (matchesEffective(staged, effective)) return modelLabel(effective)
  return stagedLabel(staged)
}

function matchesEffective(staged, effective) {
  if (!staged || !effective) return false
  return staged.provider_id === effective.provider_id &&
    staged.model === effective.model &&
    (staged.reasoning_effort || '') === (effective.reasoning_effort || '')
}

function modelLabel(preference) {
  if (!preference) return 'Not saved yet'
  return [
    preference.provider_name || providerName(preference.provider_id),
    preference.model_name || modelName(preference.provider_id, preference.model),
    reasoningLabel(preference.reasoning_effort)
  ].filter(Boolean).join(' · ')
}

function stagedLabel(preference) {
  if (!preference?.provider_id || !preference?.model) return 'Missing provider or model'
  return [
    providerName(preference.provider_id),
    modelName(preference.provider_id, preference.model),
    reasoningLabel(preference.reasoning_effort)
  ].filter(Boolean).join(' · ')
}

function providerName(providerId) {
  return props.providers.find((item) => item.id === providerId)?.name || providerId
}

function modelName(providerId, modelId) {
  const provider = props.providers.find((item) => item.id === providerId)
  return provider?.models?.find((item) => item.id === modelId)?.name || modelId
}

function save() {
  error.value = ''
  if (!canSave.value) {
    error.value = 'Provider and model are required for every role.'
    return
  }
  emit('save', {
    planner: preferencePayload(forms.planner),
    explorer: preferencePayload(forms.explorer),
    executor: preferencePayload(forms.executor),
    reviewer: preferencePayload(forms.reviewer)
  })
}

function preferencePayload(preference) {
  return {
    provider_id: preference.provider_id,
    model: preference.model,
    reasoning_effort: preference.reasoning_effort || null
  }
}
</script>
