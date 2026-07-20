<template>
  <section class="settings-section-panel">
    <div class="settings-section-header">
      <div>
        <h2>Web Search</h2>
        <p>PL resolves the effective hosted or standalone path for every agent role.</p>
      </div>
      <button class="ghost-button" type="button" :disabled="state.loading" @click="$emit('reload')">Reload</button>
    </div>

    <form class="settings-console" @submit.prevent="save">
      <div class="form-grid">
        <label>
          <span>Mode</span>
          <select v-model="form.mode">
            <option value="disabled">Disabled</option>
            <option value="cached">Cached</option>
            <option value="indexed">Indexed</option>
            <option value="live">Live</option>
          </select>
        </label>
        <label>
          <span>Context Size</span>
          <select v-model="form.context_size">
            <option value="">Provider default</option>
            <option value="low">Low</option>
            <option value="medium">Medium</option>
            <option value="high">High</option>
          </select>
        </label>
        <label class="span-2">
          <span>Allowed Domains</span>
          <input v-model="domains" placeholder="example.com, docs.example.com" />
        </label>
        <label><span>Country</span><input v-model.trim="form.country" placeholder="US" /></label>
        <label><span>Region</span><input v-model.trim="form.region" placeholder="California" /></label>
        <label><span>City</span><input v-model.trim="form.city" placeholder="San Francisco" /></label>
        <label><span>Timezone</span><input v-model.trim="form.timezone" placeholder="America/Los_Angeles" /></label>
      </div>

      <div class="mcp-summary-grid">
        <div v-for="(resolution, role) in state.roles" :key="role" class="settings-summary-item">
          <span>{{ role }}</span>
          <strong>{{ resolution.path || resolution.availability }}</strong>
          <small>
            {{ resolution.configured_mode }} → {{ resolution.effective_mode }} ·
            {{ resolution.provider_id || 'No provider' }} · {{ resolution.model || 'No model' }}
          </small>
        </div>
      </div>

      <p v-if="state.error" class="dialog-error">{{ state.error }}</p>
      <div class="settings-actions">
        <span></span>
        <button class="primary-button" type="submit" :disabled="state.saving">
          <span v-if="state.saving" class="spinner-sm"></span>
          <template v-else>Save Web Search</template>
        </button>
      </div>
    </form>
  </section>
</template>

<script setup>
import { reactive, ref, watch } from 'vue'

const props = defineProps({ state: { type: Object, required: true } })
const emit = defineEmits(['save', 'reload'])
const domains = ref('')
const form = reactive({
  mode: 'cached',
  context_size: '',
  country: '',
  region: '',
  city: '',
  timezone: ''
})

watch(
  () => props.state.config,
  (config) => {
    if (!config) return
    form.mode = config.mode || 'cached'
    form.context_size = config.context_size || ''
    domains.value = (config.allowed_domains || []).join(', ')
    form.country = config.location?.country || ''
    form.region = config.location?.region || ''
    form.city = config.location?.city || ''
    form.timezone = config.location?.timezone || ''
  },
  { immediate: true }
)

function save() {
  const location = {
    country: form.country || null,
    region: form.region || null,
    city: form.city || null,
    timezone: form.timezone || null
  }
  emit('save', {
    mode: form.mode,
    context_size: form.context_size || null,
    allowed_domains: domains.value.split(',').map((value) => value.trim()).filter(Boolean),
    location: Object.values(location).some(Boolean) ? location : null
  })
}
</script>
