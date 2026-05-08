<template>
  <div class="question-bar">
    <div class="question-bar-header">
      <strong v-if="header">{{ header }}</strong>
      <span class="question-progress">{{ currentIndex + 1 }} / {{ questions.length }}</span>
    </div>

    <div v-if="currentQuestion" class="question-bar-body">
      <p class="question-text">{{ currentQuestion.question }}</p>
      <div class="question-options">
        <button
          v-for="opt in currentQuestion.options"
          :key="opt.label"
          class="question-option"
          :disabled="sending"
          @click="selectOption(opt.label)"
        >
          <span class="option-label">{{ opt.label }}</span>
          <span v-if="opt.description" class="option-desc">{{ opt.description }}</span>
        </button>
        <button
          class="question-option question-option-other"
          :disabled="sending"
          @click="showOtherInput = true"
        >
          <span class="option-label">Other...</span>
        </button>
      </div>

      <div v-if="showOtherInput" class="question-other-input">
        <textarea
          v-model="otherText"
          rows="2"
          placeholder="Type your answer..."
          @keydown.enter.exact.prevent="submitOther"
        />
        <button
          class="primary-button"
          type="button"
          :disabled="!otherText.trim() || sending"
          @click="submitOther"
        >
          <span v-if="sending" class="spinner-sm"></span>
          <template v-else>Submit</template>
        </button>
      </div>
    </div>
  </div>
</template>

<script setup>
import { computed, ref } from 'vue'

const props = defineProps({
  questions: { type: Array, required: true },
  header: { type: String, default: '' },
  sending: { type: Boolean, default: false }
})

const emit = defineEmits(['answer'])

const currentIndex = ref(0)
const answers = new Map()
const showOtherInput = ref(false)
const otherText = ref('')

const currentQuestion = computed(() => props.questions[currentIndex.value] || null)

function selectOption(label) {
  showOtherInput.value = false
  otherText.value = ''
  advance(currentQuestion.value.id, label)
}

function submitOther() {
  const text = otherText.value.trim()
  if (!text) return
  advance(currentQuestion.value.id, text)
  showOtherInput.value = false
  otherText.value = ''
}

function advance(questionId, answer) {
  answers.set(questionId, answer)
  if (currentIndex.value + 1 >= props.questions.length) {
    emit('answer', formatResponse())
    return
  }
  currentIndex.value++
}

function formatResponse() {
  const parts = []
  if (props.header) parts.push(props.header)
  props.questions.forEach((q, i) => {
    parts.push(`Q${i + 1}: ${q.question}`)
    parts.push(`A: ${answers.get(q.id) || ''}`)
  })
  return parts.join('\n\n')
}
</script>
