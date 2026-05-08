<template>
  <div class="question-bar">
    <div class="question-bar-header">
      <strong>Plan v{{ planVersion }} ready for review</strong>
    </div>

    <div class="question-options">
      <button
        class="question-option"
        :disabled="approving"
        @click="$emit('approve')"
      >
        <span class="option-label">Approve and Execute</span>
        <span class="option-desc">Start executor and reviewer workflow</span>
      </button>
      <button
        class="question-option question-option-other"
        :disabled="approving"
        @click="showRevisionInput = true"
      >
        <span class="option-label">Request Revision...</span>
      </button>
    </div>

    <div v-if="showRevisionInput" class="question-other-input">
      <textarea
        v-model="revisionText"
        rows="2"
        placeholder="Describe what should change in the plan..."
        @keydown.enter.exact.prevent="submitRevision"
      />
      <button
        class="primary-button"
        type="button"
        :disabled="!revisionText.trim() || submittingRevision"
        @click="submitRevision"
      >
        <span v-if="submittingRevision" class="spinner-sm"></span>
        <template v-else>Submit</template>
      </button>
    </div>
  </div>
</template>

<script setup>
import { ref } from 'vue'

defineProps({
  planVersion: { type: Number, default: 0 },
  approving: { type: Boolean, default: false },
  submittingRevision: { type: Boolean, default: false }
})

const emit = defineEmits(['approve', 'request-revision'])

const showRevisionInput = ref(false)
const revisionText = ref('')

function submitRevision() {
  const text = revisionText.value.trim()
  if (!text) return
  emit('request-revision', text)
  revisionText.value = ''
  showRevisionInput.value = false
}
</script>
