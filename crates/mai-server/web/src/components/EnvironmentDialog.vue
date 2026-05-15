<template>
  <div v-if="dialog.open" class="modal-backdrop" @mousedown.self="onBackdropDown" @mouseup.self="onBackdropUp($event, () => $emit('close'))">
    <form class="modal" @submit.prevent="$emit('create')">
      <div class="modal-title-row">
        <h2>Create Environment</h2>
        <button class="icon-button" type="button" title="Close" @click="$emit('close')">×</button>
      </div>
      <label>
        <span>Environment Name</span>
        <input
          v-model.trim="dialog.name"
          placeholder="Rust workspace"
          required
        />
      </label>
      <label>
        <span>Docker Image</span>
        <input
          v-model.trim="dialog.docker_image"
          placeholder="ghcr.io/rcore-os/tgoskits-container:latest"
        />
      </label>
      <p v-if="dialog.error" class="dialog-error">{{ dialog.error }}</p>
      <div class="modal-actions">
        <button class="ghost-button" type="button" @click="$emit('close')">Cancel</button>
        <button class="primary-button" type="submit" :disabled="dialog.submitting || !dialog.name.trim()">
          <span v-if="dialog.submitting" class="spinner-sm"></span>
          <template v-else>Create</template>
        </button>
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

defineEmits(['close', 'create'])
</script>
