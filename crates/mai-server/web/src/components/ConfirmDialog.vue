<template>
  <div v-if="open" class="modal-backdrop">
    <div class="modal confirm-modal">
      <h2>{{ title }}</h2>
      <p class="confirm-message">{{ message }}</p>
      <div class="modal-actions">
        <button class="ghost-button" @click="$emit('close')">Cancel</button>
        <button class="danger-button" @click="$emit('confirm')">Confirm</button>
      </div>
    </div>
  </div>
</template>

<script setup>
defineProps({
  open: { type: Boolean, required: true },
  title: { type: String, default: '' },
  message: { type: String, default: '' }
})

defineEmits(['close', 'confirm'])
</script>
