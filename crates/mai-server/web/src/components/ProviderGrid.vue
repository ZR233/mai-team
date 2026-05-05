<template>
  <section class="providers-stage">
    <div class="stage-title">
      <div>
        <h2>Providers</h2>
        <p>Configure built-in provider presets and custom model metadata for task agents.</p>
      </div>
      <button class="primary-button" @click="$emit('add')">Add Provider</button>
    </div>

    <div class="provider-grid">
      <article
        v-for="(provider, index) in providers"
        :key="provider.id"
        class="provider-card"
        :class="{ disabled: !provider.enabled }"
      >
        <div class="provider-card-head">
          <div class="provider-main">
            <div class="provider-icon">{{ initial(provider.name) }}</div>
            <div class="provider-title-block">
              <h3>{{ provider.name }}</h3>
              <p class="mono">{{ provider.id }} / {{ providerKind(provider.kind) }}</p>
            </div>
          </div>
          <div class="provider-badges">
            <span v-if="defaultId === provider.id" class="provider-badge default">Default</span>
            <span class="provider-badge" :class="provider.enabled ? 'ready' : 'warn'">
              {{ provider.enabled ? 'Enabled' : 'Disabled' }}
            </span>
          </div>
        </div>

        <div class="provider-metrics">
          <div class="provider-metric" :class="provider.has_api_key ? 'ready' : 'danger'">
            <span>API Key</span>
            <strong>{{ provider.has_api_key ? 'Set' : 'Missing' }}</strong>
            <small>{{ provider.api_key_env || 'No env fallback' }}</small>
          </div>
          <div class="provider-metric">
            <span>Models</span>
            <strong>{{ modelCount(provider) }}</strong>
            <small>{{ modelCount(provider) === 1 ? 'configured model' : 'configured models' }}</small>
          </div>
          <div class="provider-metric">
            <span>Default Model</span>
            <strong>{{ provider.default_model || '-' }}</strong>
            <small>{{ defaultModelLabel(provider) }}</small>
          </div>
        </div>

        <div class="provider-url">
          <span>Base URL</span>
          <strong>{{ provider.base_url }}</strong>
        </div>

        <div class="model-chips">
          <span
            v-for="model in visibleModels(provider)"
            :key="model.id"
            class="model-chip"
            :class="{ default: model.id === provider.default_model }"
          >
            {{ model.name || model.id }}
          </span>
          <span v-if="hiddenModelCount(provider)" class="model-chip muted">
            +{{ hiddenModelCount(provider) }} more
          </span>
          <span v-if="!modelCount(provider)" class="model-chip muted">No models</span>
        </div>

        <div class="provider-actions">
          <button class="ghost-button" @click="$emit('edit', index)">Edit</button>
          <button class="danger-button" @click="$emit('delete', index, provider.name)">Delete</button>
        </div>
      </article>
    </div>

    <div v-if="!providers.length" class="empty-stage providers-empty">
      <div class="empty-mark">P</div>
      <h2>No providers configured</h2>
      <p>Add a provider before creating tasks.</p>
      <button class="primary-button" @click="$emit('add')">Add Provider</button>
    </div>
  </section>
</template>

<script setup>
import { initial } from '../utils/format'

const MAX_VISIBLE_MODELS = 4

defineProps({
  providers: { type: Array, required: true },
  defaultId: { type: String, default: null }
})

defineEmits(['add', 'edit', 'delete'])

function visibleModels(provider) {
  return (provider.models || []).slice(0, MAX_VISIBLE_MODELS)
}

function hiddenModelCount(provider) {
  return Math.max((provider.models || []).length - MAX_VISIBLE_MODELS, 0)
}

function modelCount(provider) {
  return (provider.models || []).length
}

function providerKind(kind) {
  return String(kind || 'custom').toUpperCase()
}

function defaultModelLabel(provider) {
  if (!provider.default_model) return 'not selected'
  return (provider.models || []).some((model) => model.id === provider.default_model)
    ? 'ready for task agents'
    : 'not in model list'
}
</script>
