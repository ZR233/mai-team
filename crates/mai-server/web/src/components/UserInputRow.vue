<template>
  <div class="timeline-row-card user-input-row">
    <div class="timeline-row-content">
      <div class="timeline-row-header">
        <div class="timeline-row-title">
          <strong>{{ item.header || 'Needs confirmation' }}</strong>
        </div>
      </div>
      <div v-for="question in item.questions" :key="question.id || question.question" class="user-input-question">
        <p>{{ question.question }}</p>
        <div class="user-input-options">
          <span
            v-for="option in question.options"
            :key="option.label"
            class="user-input-option"
            :title="option.description"
          >
            {{ option.label }}
          </span>
        </div>
      </div>
    </div>
  </div>
</template>

<script setup>
defineProps({
  item: { type: Object, required: true }
})
</script>
