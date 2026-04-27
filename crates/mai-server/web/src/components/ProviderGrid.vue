<template>
  <section class="providers-stage">
    <div class="stage-title">
      <div>
        <h2>Providers</h2>
        <p>Configure OpenAI-compatible endpoints and the model list offered to new agents.</p>
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
        <div class="provider-main">
          <div class="provider-icon">{{ initial(provider.name) }}</div>
          <div>
            <div class="provider-name-line">
              <h3>{{ provider.name }}</h3>
              <span v-if="defaultId === provider.id" class="mini-pill">Default</span>
              <span class="mini-pill" :class="provider.enabled ? 'green' : 'amber'">
                {{ provider.enabled ? 'Enabled' : 'Disabled' }}
              </span>
            </div>
            <p class="mono">{{ provider.id }}</p>
          </div>
        </div>
        <div class="provider-url">{{ provider.base_url }}</div>
        <div class="provider-stats">
          <span :class="provider.has_api_key ? 'good' : 'bad'">
            {{ provider.has_api_key ? 'API key set' : 'No API key' }}
          </span>
          <span>{{ provider.models.length }} model{{ provider.models.length === 1 ? '' : 's' }}</span>
          <span>Default model: {{ provider.default_model }}</span>
        </div>
        <div class="model-chips">
          <span
            v-for="model in provider.models"
            :key="model"
            class="model-chip"
            :class="{ default: model === provider.default_model }"
          >
            {{ model }}
          </span>
          <span v-if="!provider.models.length" class="model-chip muted">No models</span>
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
      <p>Add a provider before creating agents.</p>
      <button class="primary-button" @click="$emit('add')">Add Provider</button>
    </div>
  </section>
</template>

<script setup>
import { initial } from '../utils/format'

defineProps({
  providers: { type: Array, required: true },
  defaultId: { type: String, default: null }
})

defineEmits(['add', 'edit', 'delete'])
</script>
