<template>
  <div class="session-strip">
    <div class="session-tabs" aria-label="Chat sessions">
      <button
        v-for="session in sessions"
        :key="session.id"
        type="button"
        class="session-tab"
        :class="{ active: session.id === selectedSessionId }"
        @click="$emit('select-session', session.id)"
      >
        <span>{{ session.title }}</span>
        <small>{{ session.message_count }}</small>
      </button>
    </div>
    <button class="small-button" type="button" @click="$emit('create-session')">New Session</button>
  </div>
</template>

<script setup>
defineProps({
  sessions: { type: Array, default: () => [] },
  selectedSessionId: { type: String, default: '' }
})

defineEmits(['select-session', 'create-session'])
</script>
