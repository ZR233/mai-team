<template>
  <div v-if="dialog.open" class="modal-backdrop" @mousedown.self="onBackdropDown" @mouseup.self="onBackdropUp($event, () => $emit('close'))">
    <form class="modal" @submit.prevent="$emit('create')">
      <div class="modal-title-row">
        <h2>Create Task</h2>
        <button class="icon-button" type="button" @click="$emit('close')">Close</button>
      </div>
      <label>
        <span>Task Title</span>
        <input v-model.trim="dialog.title" placeholder="Implement task workflow" />
      </label>
      <label>
        <span>Initial Message</span>
        <textarea
          v-model.trim="dialog.message"
          rows="4"
          placeholder="Describe the task for the planner..."
        ></textarea>
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
        <button class="primary-button" type="submit">Create</button>
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
