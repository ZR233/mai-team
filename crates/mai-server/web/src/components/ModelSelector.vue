<template>
  <div class="model-selector" :class="{ compact }">
    <label>
      <span>Provider</span>
      <select :value="providerId" :disabled="disabled || !providers.length" @change="onProviderChange">
        <option v-for="provider in providers" :key="provider.id" :value="provider.id">
          {{ provider.name }}
        </option>
      </select>
    </label>
    <label>
      <span>Model</span>
      <select :value="model" :disabled="disabled || !models.length" @change="onModelChange">
        <option v-for="item in models" :key="item.id" :value="item.id">
          {{ item.name || item.id }}
        </option>
      </select>
    </label>
    <label v-if="reasoningOptions.length">
      <span>思考深度</span>
      <select :value="reasoningEffort" :disabled="disabled" @change="$emit('update:reasoningEffort', $event.target.value)">
        <option v-for="option in reasoningOptions" :key="option.value" :value="option.value">
          {{ option.label }}
        </option>
      </select>
    </label>
  </div>
</template>

<script setup>
import { computed, watch } from 'vue'
import { defaultReasoningEffort, reasoningOptionsFor } from '../utils/reasoning'

const props = defineProps({
  providers: { type: Array, default: () => [] },
  providerId: { type: String, default: '' },
  model: { type: String, default: '' },
  reasoningEffort: { type: String, default: '' },
  disabled: { type: Boolean, default: false },
  compact: { type: Boolean, default: false },
  autoSelect: { type: Boolean, default: true }
})

const emit = defineEmits([
  'update:providerId',
  'update:model',
  'update:reasoningEffort',
  'changed'
])

const provider = computed(() => props.providers.find((item) => item.id === props.providerId))
const models = computed(() => provider.value?.models || [])
const selectedModel = computed(() => models.value.find((item) => item.id === props.model))
const reasoningOptions = computed(() => reasoningOptionsFor(provider.value, selectedModel.value))

watch(
  () => [props.providers, props.providerId, props.model],
  () => {
    if (!props.autoSelect || props.disabled) return
    const nextProvider = provider.value || props.providers[0]
    if (!nextProvider) return
    if (nextProvider.id !== props.providerId) {
      emit('update:providerId', nextProvider.id)
      return
    }
    const nextModels = nextProvider.models || []
    const currentModel = nextModels.find((item) => item.id === props.model)
    const nextModel = currentModel
      ? currentModel.id
      : nextProvider.default_model || nextModels[0]?.id || ''
    if (nextModel && nextModel !== props.model) {
      emit('update:model', nextModel)
      return
    }
    syncReasoning()
  },
  { immediate: true }
)

watch(
  () => [props.providerId, props.model, reasoningOptions.value.map((option) => option.value).join('|')],
  () => {
    if (props.autoSelect && !props.disabled) syncReasoning()
  },
  { immediate: true }
)

function onProviderChange(event) {
  const nextProviderId = event.target.value
  const nextProvider = props.providers.find((item) => item.id === nextProviderId)
  const nextModel = nextProvider?.default_model || nextProvider?.models?.[0]?.id || ''
  emit('update:providerId', nextProviderId)
  emit('update:model', nextModel)
  emit('update:reasoningEffort', defaultReasoningEffort(nextProvider, nextProvider?.models?.find((item) => item.id === nextModel)))
  emit('changed')
}

function onModelChange(event) {
  const nextModel = event.target.value
  const modelConfig = models.value.find((item) => item.id === nextModel)
  emit('update:model', nextModel)
  emit('update:reasoningEffort', defaultReasoningEffort(provider.value, modelConfig))
  emit('changed')
}

function syncReasoning() {
  const next = defaultReasoningEffort(provider.value, selectedModel.value)
  const valid = reasoningOptions.value.some((option) => option.value === props.reasoningEffort)
  if (next !== props.reasoningEffort && (!props.reasoningEffort || !valid)) {
    emit('update:reasoningEffort', next)
  }
  if (!reasoningOptions.value.length && props.reasoningEffort) {
    emit('update:reasoningEffort', '')
  }
}
</script>
