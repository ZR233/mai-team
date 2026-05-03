<template>
  <form class="composer" @submit.prevent="send">
    <textarea
      :value="draft"
      rows="2"
      placeholder="Send a command or message... (Enter to send, Shift+Enter for new line)"
      @input="$emit('update:draft', $event.target.value)"
      @keydown.enter.exact="handleEnter"
    />
    <button class="primary-button" type="submit" :disabled="!draft.trim() || sending">
      <span v-if="sending" class="spinner-sm"></span>
      <template v-else>Send</template>
    </button>
  </form>
</template>

<script setup>
const props = defineProps({
  draft: { type: String, default: '' },
  sending: { type: Boolean, default: false }
})

const emit = defineEmits(['send', 'update:draft'])

function handleEnter(event) {
  if (!event.shiftKey) {
    event.preventDefault()
    send()
  }
}

function send() {
  if (props.draft.trim()) emit('send', props.draft.trim())
}
</script>
