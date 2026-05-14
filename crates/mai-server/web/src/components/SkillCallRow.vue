<template>
  <div class="timeline-row-card skill-call-row">
    <div class="timeline-row-content">
      <div class="timeline-row-header">
        <div class="timeline-row-title">
          <strong>{{ item.title || 'Skills activated' }}</strong>
          <span v-if="item.summary">{{ item.summary }}</span>
        </div>
      </div>
      <div v-if="item.skills?.length" class="skill-chip-list">
        <span
          v-for="skill in item.skills"
          :key="skill.path || `${skill.scope}-${skill.name}`"
          class="skill-chip"
          :title="skill.path"
        >
          <span>{{ skill.name }}</span>
          <small v-if="skill.scope">{{ skill.scope }}</small>
        </span>
      </div>
    </div>
  </div>
</template>

<script setup>
defineProps({
  item: { type: Object, required: true }
})
</script>
