<template>
  <form class="composer" @submit.prevent="send">
    <div class="composer-main">
      <div v-if="selectedSkillItems.length" class="selected-skills">
        <button
          v-for="skill in selectedSkillItems"
          :key="skill.path || skill.name"
          class="skill-chip"
          type="button"
          :title="skill.path"
          @click="toggleSkill(skill)"
        >
          <span>{{ skillLabel(skill) }}</span>
          <span class="skill-chip-x">x</span>
        </button>
      </div>
      <textarea
        :value="draft"
        rows="2"
        placeholder="Send a command or message... (Enter to send, Shift+Enter for new line)"
        :disabled="disabled"
        @input="$emit('update:draft', $event.target.value)"
        @keydown.enter.exact="handleEnter"
      />
    </div>

    <div class="composer-actions">
      <div class="skill-picker-wrap">
        <button
          class="icon-button skill-picker-button"
          type="button"
          title="Skills"
          :disabled="disabled || sending || skillsLoading"
          @click="togglePicker"
        >
          @
          <span v-if="selectedSkillItems.length" class="skill-count">{{ selectedSkillItems.length }}</span>
        </button>
        <div v-if="pickerOpen" class="skill-picker">
          <div class="skill-picker-head">
            <strong>Skills</strong>
            <input v-model="query" type="search" placeholder="Filter skills..." />
          </div>
          <div v-if="skillsError" class="skill-picker-error">{{ skillsError }}</div>
          <div v-else-if="skillsLoading" class="skill-picker-empty">Loading skills...</div>
          <div v-else-if="!filteredSkills.length" class="skill-picker-empty">No matching skills.</div>
          <button
            v-for="skill in filteredSkills"
            v-else
            :key="skill.path || skill.name"
            class="skill-option"
            type="button"
            :class="{ selected: isSelected(skill), disabled: !skill.enabled }"
            :disabled="!skill.enabled"
            @click="toggleSkill(skill)"
          >
            <span class="skill-option-mark">{{ isSelected(skill) ? '✓' : '' }}</span>
            <span class="skill-option-main">
              <strong>{{ skillLabel(skill) }}</strong>
              <small>{{ skill.description || skill.short_description || skill.path }}</small>
            </span>
            <span class="mini-pill">{{ skill.scope }}</span>
          </button>
        </div>
      </div>
      <button class="primary-button" type="submit" :disabled="disabled || !draft.trim() || sending || stoppable">
        <span v-if="sending" class="spinner-sm"></span>
        <template v-else>Send</template>
      </button>
      <button
        v-if="stoppable"
        class="danger-button"
        type="button"
        :disabled="stopping"
        @click="$emit('stop')"
      >
        <span v-if="stopping" class="spinner-sm"></span>
        <template v-else>Stop</template>
      </button>
    </div>
  </form>
</template>

<script setup>
import { computed, ref } from 'vue'

const props = defineProps({
  draft: { type: String, default: '' },
  sending: { type: Boolean, default: false },
  skills: { type: Array, default: () => [] },
  selectedSkills: { type: Array, default: () => [] },
  skillsLoading: { type: Boolean, default: false },
  skillsError: { type: String, default: '' },
  stoppable: { type: Boolean, default: false },
  stopping: { type: Boolean, default: false },
  disabled: { type: Boolean, default: false }
})

const emit = defineEmits(['send', 'update:draft', 'update:selectedSkills', 'load-skills', 'stop'])

const pickerOpen = ref(false)
const query = ref('')

const nameCounts = computed(() => {
  const counts = new Map()
  for (const skill of props.skills) counts.set(skill.name, (counts.get(skill.name) || 0) + 1)
  return counts
})

const selectedSet = computed(() => new Set(props.selectedSkills))
const selectedSkillItems = computed(() =>
  props.selectedSkills
    .map((value) => props.skills.find((skill) => skill.path === value || skill.name === value) || { name: value, path: value })
)

const filteredSkills = computed(() => {
  const needle = query.value.trim().toLowerCase()
  return props.skills.filter((skill) => {
    if (!needle) return true
    return [skill.name, skill.description, skill.path]
      .filter(Boolean)
      .some((value) => value.toLowerCase().includes(needle))
  })
})

function handleEnter(event) {
  if (!event.shiftKey) {
    event.preventDefault()
    send()
  }
}

function send() {
  if (!props.disabled && props.draft.trim()) {
    emit('send', {
      message: props.draft.trim(),
      skillMentions: [...props.selectedSkills]
    })
    pickerOpen.value = false
  }
}

function togglePicker() {
  pickerOpen.value = !pickerOpen.value
  if (pickerOpen.value) emit('load-skills')
}

function skillMentionValue(skill) {
  return skill.scope === 'project' ? (skill.path || skill.name) : (nameCounts.value.get(skill.name) > 1 ? skill.path : (skill.path || skill.name))
}

function isSelected(skill) {
  return selectedSet.value.has(skillMentionValue(skill)) || selectedSet.value.has(skill.path) || selectedSet.value.has(skill.name)
}

function toggleSkill(skill) {
  const value = skillMentionValue(skill)
  const next = props.selectedSkills.filter((item) => item !== value && item !== skill.path && item !== skill.name)
  if (!isSelected(skill)) next.push(value)
  emit('update:selectedSkills', next)
}

function skillLabel(skill) {
  if (!skill.name) return skill.path || 'Skill'
  return nameCounts.value.get(skill.name) > 1 && skill.scope ? `${skill.name} · ${skill.scope}` : skill.name
}
</script>
