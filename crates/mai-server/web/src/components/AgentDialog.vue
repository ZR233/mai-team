<template>
  <div v-if="dialog.open" class="modal-backdrop" @mousedown.self="onBackdropDown" @mouseup.self="onBackdropUp($event, () => $emit('close'))">
    <form class="modal" @submit.prevent="$emit('create')">
      <div class="modal-title-row">
        <h2>Create Agent</h2>
        <button class="icon-button" type="button" @click="$emit('close')">Close</button>
      </div>
      <label>
        <span>Agent Name</span>
        <input v-model.trim="dialog.name" placeholder="Research Agent" />
      </label>
      <label>
        <span>Provider</span>
        <select v-model="dialog.provider_id" @change="$emit('provider-changed')">
          <option v-for="provider in providers" :key="provider.id" :value="provider.id">
            {{ provider.name }}
          </option>
        </select>
      </label>
      <label>
        <span>Model</span>
        <select v-model="dialog.model">
          <option v-for="model in models" :key="model.id" :value="model.id">
            {{ model.name || model.id }}
          </option>
        </select>
      </label>
      <p v-if="dialog.error" class="dialog-error">{{ dialog.error }}</p>
      <div class="modal-actions">
        <button class="ghost-button" type="button" @click="$emit('close')">Cancel</button>
        <button class="primary-button" type="submit" :disabled="!providers.length">Create</button>
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
  dialog: { type: Object, required: true },
  providers: { type: Array, required: true },
  models: { type: Array, required: true }
})

defineEmits(['close', 'create', 'provider-changed'])
</script>
