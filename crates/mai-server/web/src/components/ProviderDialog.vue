<template>
  <div v-if="dialog.open" class="modal-backdrop" @mousedown.self="onBackdropDown" @mouseup.self="onBackdropUp($event, () => $emit('close'))">
    <form class="modal wide" @submit.prevent="$emit('save')">
      <div class="modal-title-row">
        <h2>{{ dialog.index === null ? 'Add Provider' : 'Edit Provider' }}</h2>
        <button class="icon-button" type="button" @click="$emit('close')">Close</button>
      </div>
      <div class="form-grid">
        <label>
          <span>Model Provider</span>
          <select v-model="dialog.form.preset_id" @change="$emit('kind-changed', dialog.form.preset_id)">
            <option
              v-for="preset in presets"
              :key="preset.id"
              :value="preset.id"
            >
              {{ preset.display_name }}
            </option>
            <option value="">Custom provider</option>
          </select>
        </label>
        <label>
          <span>Provider ID</span>
          <input v-model.trim="dialog.form.id" placeholder="provider-id" />
        </label>
        <label v-if="!dialog.form.preset_id">
          <span>Wire Protocol</span>
          <select v-model="dialog.form.protocol" @change="onProtocolChanged">
            <option v-for="protocol in protocolOptions" :key="protocol.id" :value="protocol.id">
              {{ protocol.label }}
            </option>
          </select>
        </label>
        <label>
          <span>Display Name</span>
          <input v-model.trim="dialog.form.name" placeholder="Provider name" />
        </label>
        <label class="span-2">
          <span>Base URL</span>
          <input v-model.trim="dialog.form.base_url" placeholder="https://api.example.com/v1" />
        </label>
        <div v-if="dialog.form.connection_modes.length" class="span-2 provider-connection-field">
          <span>Connection</span>
          <div
            class="segmented-control provider-connection-tabs"
            :style="{ gridTemplateColumns: `repeat(${dialog.form.connection_modes.length}, minmax(0, 1fr))` }"
            role="tablist"
            aria-label="Provider connection mode"
          >
            <button
              v-for="mode in dialog.form.connection_modes"
              :key="mode.id"
              type="button"
              role="tab"
              :aria-selected="dialog.form.connection_mode === mode.id"
              :class="{ active: dialog.form.connection_mode === mode.id }"
              :disabled="dialog.form.connection_modes.length === 1"
              @click="dialog.form.connection_mode = mode.id"
            >
              {{ mode.display_name }}
            </button>
          </div>
        </div>
        <label>
          <span>{{ dialog.form.credential_label || 'API Key' }}</span>
          <input
            v-model="dialog.form.api_key"
            type="password"
            :placeholder="dialog.form.has_api_key ? 'Leave blank to keep existing key' : 'sk-...'"
          />
        </label>
        <label>
          <span>API Key Env</span>
          <input v-model.trim="dialog.form.api_key_env" placeholder="PROVIDER_API_KEY" />
        </label>
        <label>
          <span>Default Model</span>
          <input
            v-model.trim="dialog.form.default_model"
            list="provider-dialog-models"
            placeholder="Select or enter a model"
          />
          <datalist id="provider-dialog-models">
            <option
              v-for="model in dialog.form.effectiveModels"
              :key="model.id"
              :value="model.id"
            >
              {{ model.name || model.id }}
            </option>
          </datalist>
        </label>
        <label class="span-2">
          <span>{{ dialog.form.catalog_source === 'bundled' ? 'Additional Models JSON' : 'Models JSON' }}</span>
          <textarea
            v-model="dialog.form.modelsText"
            rows="10"
            placeholder='[{"id":"model-id","context_tokens":128000,"effective_context_window_percent":95,"output_tokens":8192}]'
          />
        </label>
        <label>
          <span>Enabled</span>
          <select v-model="dialog.form.enabled">
            <option :value="true">Enabled</option>
            <option :value="false">Disabled</option>
          </select>
        </label>
        <label>
          <span>Default Provider</span>
          <select v-model="dialog.form.default_provider">
            <option :value="false">No</option>
            <option :value="true">Yes</option>
          </select>
        </label>
      </div>
      <p v-if="dialog.error" class="dialog-error">{{ dialog.error }}</p>
      <div class="modal-actions">
        <button class="ghost-button" type="button" @click="$emit('close')">Cancel</button>
        <button class="primary-button" type="submit">Save Provider</button>
      </div>
    </form>
  </div>
</template>

<script setup>
import { computed } from 'vue'

let backdropDown = false

function onBackdropDown() {
  backdropDown = true
}

function onBackdropUp(_event, callback) {
  if (backdropDown) {
    backdropDown = false
    callback()
  }
}

const props = defineProps({
  dialog: { type: Object, required: true },
  presets: { type: Array, default: () => [] }
})

defineEmits(['close', 'save', 'kind-changed'])

const protocolOptions = computed(() => {
  const protocols = new Map()
  for (const preset of props.presets) {
    const transport = preset.transport
    if (!transport?.protocol || protocols.has(transport.protocol)) continue
    protocols.set(transport.protocol, {
      id: transport.protocol,
      label: transport.protocol
        .replaceAll('_', ' ')
        .replace(/\b\w/g, (letter) => letter.toUpperCase()),
      modes: transport.connection_modes || [],
      defaultMode: transport.connection_modes?.some((mode) => mode.id === 'http')
        ? 'http'
        : transport.default_connection_mode
    })
  }
  return [...protocols.values()]
})

function onProtocolChanged() {
  const protocol = protocolOptions.value.find(
    (candidate) => candidate.id === props.dialog.form.protocol
  )
  props.dialog.form.connection_modes = protocol?.modes || []
  props.dialog.form.connection_mode = protocol?.defaultMode || 'http'
}
</script>
