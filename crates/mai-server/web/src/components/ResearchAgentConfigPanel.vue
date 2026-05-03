<template>
  <section class="settings-stage">
    <div class="stage-title">
      <div>
        <h2>Agent Config</h2>
        <p>Choose the model profile used by subagents created with spawn_agent.</p>
      </div>
      <button class="ghost-button" :disabled="loading" @click="$emit('reload')">Reload</button>
    </div>

    <div v-if="!providers.length" class="empty-stage providers-empty">
      <div class="empty-mark">P</div>
      <h2>No providers configured</h2>
      <p>Add a provider before configuring Research Agent.</p>
      <button class="primary-button" @click="$emit('open-providers')">Open Providers</button>
    </div>

    <form v-else class="settings-panel" @submit.prevent="save">
      <div class="settings-panel-head">
        <div>
          <h3>Research Agent</h3>
          <p>All child agents use this profile. Leave it on default to use the global provider default model.</p>
        </div>
        <label class="toggle-row">
          <input v-model="useDefault" type="checkbox" />
          <span>Use default model</span>
        </label>
      </div>

      <ModelSelector
        v-model:provider-id="form.provider_id"
        v-model:model="form.model"
        v-model:reasoning-effort="form.reasoning_effort"
        :providers="providers"
        :disabled="useDefault || saving"
      />

      <div class="effective-model-line">
        <span>Effective</span>
        <strong>{{ effectiveLabel }}</strong>
      </div>
      <p v-if="state.validation_error" class="dialog-error">{{ state.validation_error }}</p>
      <p v-if="error" class="dialog-error">{{ error }}</p>

      <div class="settings-actions">
        <button class="ghost-button" type="button" :disabled="saving" @click="resetFromState">Reset</button>
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
import { reasoningLabel } from '../utils/reasoning'

const props = defineProps({
  providers: { type: Array, default: () => [] },
  state: { type: Object, required: true },
  loading: { type: Boolean, default: false },
  saving: { type: Boolean, default: false }
})

const emit = defineEmits(['save', 'reload', 'open-providers'])

const useDefault = ref(true)
const error = ref('')
const form = reactive({
  provider_id: '',
  model: '',
  reasoning_effort: ''
})

const effectiveLabel = computed(() => {
  const effective = props.state.effective_research_agent
  if (!effective) return 'No effective model'
  return [
    effective.provider_name || effective.provider_id,
    effective.model_name || effective.model,
    reasoningLabel(effective.reasoning_effort)
  ].filter(Boolean).join(' · ')
})

const canSave = computed(() => useDefault.value || (form.provider_id && form.model))

watch(
  () => [props.state.research_agent, props.providers],
  resetFromState,
  { immediate: true }
)

function resetFromState() {
  const configured = props.state.research_agent
  useDefault.value = !configured
  const provider = configured
    ? props.providers.find((item) => item.id === configured.provider_id)
    : props.providers[0]
  form.provider_id = configured?.provider_id || provider?.id || ''
  form.model = configured?.model || provider?.default_model || provider?.models?.[0]?.id || ''
  form.reasoning_effort = configured?.reasoning_effort || ''
  error.value = ''
}

function save() {
  error.value = ''
  if (!useDefault.value && (!form.provider_id || !form.model)) {
    error.value = 'Provider and model are required.'
    return
  }
  emit('save', useDefault.value ? null : {
    provider_id: form.provider_id,
    model: form.model,
    reasoning_effort: form.reasoning_effort || null
  })
}
</script>
