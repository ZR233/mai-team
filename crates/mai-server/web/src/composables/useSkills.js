import { computed, reactive } from 'vue'
import { useApi } from './useApi'

const skillsState = reactive({
  roots: [],
  skills: [],
  errors: [],
  loading: false,
  saving: false,
  loaded: false
})

export function useSkills() {
  const { api } = useApi()

  async function loadSkills() {
    skillsState.loading = true
    try {
      const response = await api('/skills')
      applySkillsResponse(response)
      return response
    } finally {
      skillsState.loading = false
    }
  }

  async function ensureSkillsLoaded() {
    if (!skillsState.loaded && !skillsState.loading) await loadSkills()
  }

  async function saveSkillsConfig(config) {
    skillsState.saving = true
    try {
      const response = await api('/skills/config', {
        method: 'PUT',
        body: JSON.stringify({ config })
      })
      applySkillsResponse(response)
      return response
    } finally {
      skillsState.saving = false
    }
  }

  const enabledSkills = computed(() => skillsState.skills.filter((skill) => skill.enabled))

  return {
    skillsState,
    enabledSkills,
    loadSkills,
    ensureSkillsLoaded,
    saveSkillsConfig
  }
}

function applySkillsResponse(response) {
  skillsState.roots = response?.roots || []
  skillsState.skills = response?.skills || []
  skillsState.errors = response?.errors || []
  skillsState.loaded = true
}
