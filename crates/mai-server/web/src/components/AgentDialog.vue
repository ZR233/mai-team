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
        <span>Docker Image</span>
        <input
          v-model.trim="dialog.docker_image"
          placeholder="ghcr.io/rcore-os/tgoskits-container:latest"
        />
      </label>
      <ModelSelector
        v-model:provider-id="dialog.provider_id"
        v-model:model="dialog.model"
        v-model:reasoning-effort="dialog.reasoning_effort"
        :providers="providers"
      />
      <p v-if="dialog.error" class="dialog-error">{{ dialog.error }}</p>
      <div class="modal-actions">
        <button class="ghost-button" type="button" @click="$emit('close')">Cancel</button>
        <button class="primary-button" type="submit" :disabled="!providers.length">Create</button>
      </div>
    </form>
  </div>
</template>

<script setup>
import ModelSelector from './ModelSelector.vue'

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
  models: { type: Array, default: () => [] },
  reasoningOptions: { type: Array, default: () => [] }
})

defineEmits(['close', 'create', 'provider-changed', 'model-changed'])
</script>
