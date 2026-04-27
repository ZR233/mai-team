<template>
  <div v-if="dialog.open" class="modal-backdrop" @mousedown.self="onBackdropDown" @mouseup.self="onBackdropUp($event, () => $emit('close'))">
    <form class="modal wide" @submit.prevent="$emit('save')">
      <div class="modal-title-row">
        <h2>{{ dialog.index === null ? 'Add Provider' : 'Edit Provider' }}</h2>
        <button class="icon-button" type="button" @click="$emit('close')">Close</button>
      </div>
      <div class="form-grid">
        <label>
          <span>Provider ID</span>
          <input v-model.trim="dialog.form.id" placeholder="openai" />
        </label>
        <label>
          <span>Display Name</span>
          <input v-model.trim="dialog.form.name" placeholder="OpenAI" />
        </label>
        <label class="span-2">
          <span>OpenAI Base URL</span>
          <input v-model.trim="dialog.form.base_url" placeholder="https://api.openai.com/v1" />
        </label>
        <label>
          <span>API Key</span>
          <input
            v-model="dialog.form.api_key"
            type="password"
            :placeholder="dialog.form.has_api_key ? 'Leave blank to keep existing key' : 'sk-...'"
          />
        </label>
        <label>
          <span>Default Model</span>
          <input v-model.trim="dialog.form.default_model" placeholder="gpt-5.2" />
        </label>
        <label class="span-2">
          <span>Models</span>
          <textarea v-model="dialog.form.modelsText" rows="5" placeholder="One model per line" />
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

defineProps({
  dialog: { type: Object, required: true }
})

defineEmits(['close', 'save'])
</script>
