<template>
  <section class="settings-stage">
    <div class="settings-page-title">
      <div>
        <h2>Settings</h2>
        <p>Tune role models, skills, and Git accounts for task agents.</p>
      </div>
    </div>

    <div class="settings-shell">
      <aside class="settings-sidebar" aria-label="Settings sections">
        <button
          v-for="section in SETTINGS_SECTIONS"
          :key="section.key"
          class="settings-nav-item"
          type="button"
          :class="{ active: activeSettingsSection === section.key }"
          @click="activeSettingsSection = section.key"
        >
          <span class="settings-nav-icon" aria-hidden="true">{{ section.icon }}</span>
          <span class="settings-nav-copy">
            <strong>{{ section.title }}</strong>
            <small>{{ sectionStatus(section.key) }}</small>
          </span>
        </button>
      </aside>

      <div class="settings-content">
        <section v-if="activeSettingsSection === 'roles'" class="settings-section-panel role-models-section">
          <div class="settings-section-header">
            <div>
              <h2>Role Models</h2>
              <p>Assign model profiles to planner, explorer, executor, and reviewer task-agent roles.</p>
            </div>
            <span class="section-status" :class="canSave ? 'ready' : 'danger'">
              {{ canSave ? 'Ready' : 'Incomplete' }} · {{ configuredCount }} of {{ ROLE_DEFINITIONS.length }} configured
            </span>
          </div>

          <div v-if="!providers.length" class="empty-stage providers-empty">
            <div class="empty-mark">P</div>
            <h2>No providers configured</h2>
            <p>Add a provider before configuring task-agent roles.</p>
            <button class="primary-button" type="button" @click="$emit('open-providers')">Open Providers</button>
          </div>

          <form v-else class="settings-console" @submit.prevent="save">
            <div class="settings-summary">
              <div class="settings-summary-item">
                <span>Provider seed</span>
                <strong>{{ providerSeedLabel }}</strong>
                <small>Empty roles are prefilled from the first provider.</small>
              </div>
              <div class="settings-summary-item" :class="canSave ? 'ready' : 'danger'">
                <span>Role config</span>
                <strong>{{ canSave ? 'Ready' : 'Incomplete' }}</strong>
                <small>{{ configuredCount }} of {{ ROLE_DEFINITIONS.length }} roles saved or staged.</small>
              </div>
              <div class="settings-summary-item">
                <span>Spawn default</span>
                <strong>Executor</strong>
                <small>Task workflows use executor after plan approval.</small>
              </div>
            </div>

            <div class="role-models-layout">
              <div class="role-config-list">
                <article
                  v-for="role in ROLE_DEFINITIONS"
                  :key="role.key"
                  class="role-config-row"
                  :class="`role-${role.key}`"
                >
                  <div class="role-card-head">
                    <div class="role-avatar">{{ role.initial }}</div>
                    <div>
                      <h3>{{ role.title }}</h3>
                      <p>{{ role.description }}</p>
                    </div>
                    <span class="role-status" :class="{ staged: !savedPreference(role.key) }">
                      {{ savedPreference(role.key) ? 'Configured' : 'Prefilled' }}
                    </span>
                  </div>

                  <ModelSelector
                    v-model:provider-id="forms[role.key].provider_id"
                    v-model:model="forms[role.key].model"
                    v-model:reasoning-effort="forms[role.key].reasoning_effort"
                    :providers="providers"
                    :disabled="saving"
                    class="role-model-selector"
                  />

                  <div class="effective-model-line role-effective-line">
                    <span>Effective</span>
                    <strong>{{ effectiveLabel(role.key) }}</strong>
                  </div>
                </article>
              </div>

              <aside class="role-health-panel">
                <div class="role-health-head">
                  <h3>Role health</h3>
                  <span :class="canSave ? 'mini-pill green' : 'mini-pill amber'">
                    {{ canSave ? 'Ready' : 'Needs input' }}
                  </span>
                </div>
                <div class="health-check-list">
                  <div class="health-check" :class="{ ready: canSave }">
                    <span class="health-dot"></span>
                    <span>All roles complete</span>
                  </div>
                  <div class="health-check" :class="{ ready: providersWithKeysCount > 0 }">
                    <span class="health-dot"></span>
                    <span>Provider keys present</span>
                  </div>
                  <div class="health-check" :class="{ ready: reasoningConfigured }">
                    <span class="health-dot"></span>
                    <span>Reasoning defaults valid</span>
                  </div>
                </div>
                <div class="spawn-default-card">
                  <span>Current spawn default</span>
                  <strong>Executor</strong>
                  <small>Task workflows use executor after plan approval.</small>
                </div>
              </aside>
            </div>

            <div class="settings-actions">
              <div class="settings-actions-left">
                <button class="ghost-button" type="button" :disabled="saving" @click="resetFromState">Reset</button>
                <button class="ghost-button" type="button" :disabled="loading" @click="$emit('reload')">Reload</button>
              </div>
              <div class="settings-action-errors">
                <p v-if="state.validation_error" class="dialog-error">{{ state.validation_error }}</p>
                <p v-if="error" class="dialog-error">{{ error }}</p>
              </div>
              <button class="primary-button" type="submit" :disabled="saving || !canSave">
                <span v-if="saving" class="spinner-sm"></span>
                <template v-else>Save Config</template>
              </button>
            </div>
          </form>
        </section>

        <section v-else-if="activeSettingsSection === 'skills'" class="settings-section-panel skills-settings">
          <div class="settings-section-header">
            <div>
              <h2>Skills</h2>
              <p>Codex-compatible skills discovered from repo and user roots.</p>
            </div>
            <button class="ghost-button" type="button" :disabled="skillsState.loading" @click="$emit('reload-skills')">Reload</button>
          </div>

          <div class="skills-overview">
            <div class="settings-summary-item ready">
              <span>Enabled</span>
              <strong>{{ enabledSkillCount }}</strong>
              <small>{{ disabledSkillCount }} disabled</small>
            </div>
            <div class="settings-summary-item">
              <span>Roots</span>
              <strong>{{ skillsState.roots?.length || 0 }}</strong>
              <small>Repo and user skill roots.</small>
            </div>
          </div>

          <div class="skills-root-list">
            <span v-for="root in skillsState.roots" :key="root" class="skill-root">{{ root }}</span>
            <span v-if="!skillsState.roots?.length" class="skill-root muted">No roots loaded</span>
          </div>

          <div v-if="skillsState.errors?.length" class="skill-errors">
            <div v-for="item in skillsState.errors" :key="item.path" class="skill-error-row">
              <strong>{{ item.path }}</strong>
              <span>{{ item.message }}</span>
            </div>
          </div>

          <div class="skills-config-list">
            <button
              v-for="skill in skillsState.skills"
              :key="skill.path"
              class="skill-config-row"
              type="button"
              :class="{ disabled: !skill.enabled }"
              @click="toggleSkill(skill)"
            >
              <span class="skill-toggle" :class="{ on: skill.enabled }">{{ skill.enabled ? 'On' : 'Off' }}</span>
              <span class="skill-config-main">
                <strong>{{ skill.name }}</strong>
                <small>{{ skill.description }}</small>
                <code>{{ skill.path }}</code>
              </span>
              <span class="mini-pill">{{ skill.scope }}</span>
            </button>
            <div v-if="!skillsState.loading && !skillsState.skills?.length" class="quiet-empty skills-empty">
              <strong>No skills found</strong>
              <span>Add `SKILL.md` files under `.agents/skills` or `~/.agents/skills`.</span>
            </div>
          </div>

          <div class="settings-actions">
            <div class="settings-action-errors">
              <p v-if="skillsError" class="dialog-error">{{ skillsError }}</p>
            </div>
            <button class="primary-button" type="button" :disabled="skillsSaving || skillsState.loading" @click="saveSkills">
              <span v-if="skillsSaving" class="spinner-sm"></span>
              <template v-else>Save Skills</template>
            </button>
          </div>
        </section>

        <section v-else-if="activeSettingsSection === 'git-accounts'" class="settings-section-panel github-settings">
          <div class="settings-section-header">
            <div>
              <h2>Git Accounts</h2>
              <p>Manage GitHub tokens used by project agents for repository access.</p>
            </div>
            <span class="section-status" :class="gitAccountCount ? 'ready' : ''">
              {{ gitAccountCount ? `${gitAccountCount} connected` : 'No accounts' }}
            </span>
          </div>

          <div class="settings-summary">
            <div class="settings-summary-item" :class="gitAccountCount ? 'ready' : ''">
              <span>Connected accounts</span>
              <strong>{{ gitAccountCount }}</strong>
              <small>{{ defaultGitAccount ? `Default: ${accountDisplay(defaultGitAccount)}` : 'Add a GitHub account.' }}</small>
            </div>
            <div class="settings-summary-item ready">
              <span>Provider</span>
              <strong>GitHub</strong>
              <small>GitLab is reserved for a later version.</small>
            </div>
            <div class="settings-summary-item ready">
              <span>Agent tools</span>
              <strong>Git MCP + GitHub MCP</strong>
              <small>Same stored token, server-side only.</small>
            </div>
          </div>

          <div class="git-accounts-layout">
            <div class="git-account-list">
              <button
                v-for="account in gitAccounts"
                :key="account.id"
                class="git-account-card"
                type="button"
                :class="{ active: selectedGitAccountId === account.id }"
                @click="selectGitAccount(account)"
              >
                <span class="git-account-mark">GH</span>
                <span class="git-account-main">
                  <strong>{{ account.label || 'GitHub' }}</strong>
                  <small>{{ accountDisplay(account) }}</small>
                  <span class="git-account-meta">{{ tokenKindLabel(account.token_kind) }} · {{ gitAccountStatusLabel(account.status) }}</span>
                </span>
                <span v-if="account.is_default" class="mini-pill green">Default</span>
              </button>
              <div v-if="!gitAccountCount" class="quiet-empty skills-empty">
                <strong>No Git accounts</strong>
                <span>Add a GitHub PAT to enable project repositories.</span>
              </div>
            </div>

            <form class="git-account-form" @submit.prevent="saveGitAccountForm">
              <div class="integration-card-head">
                <div>
                  <h3>{{ gitForm.id ? 'Edit Account' : 'Add Account' }}</h3>
                  <p>Token type is detected after verification.</p>
                </div>
                <span class="section-status" :class="gitAccountStatusClass(selectedGitAccount?.status)">
                  {{ selectedGitAccount ? gitAccountStatusLabel(selectedGitAccount.status) : 'New' }}
                </span>
              </div>

              <div class="segmented-control provider-control" role="group" aria-label="Git provider">
                <button class="active" type="button">GitHub</button>
                <button class="disabled" type="button" disabled>GitLab</button>
              </div>

              <label>
                <span>Account label</span>
                <input v-model.trim="gitForm.label" placeholder="Personal" autocomplete="off" />
              </label>
              <label>
                <span>Token</span>
                <input v-model="gitForm.token" type="password" placeholder="ghp_ or github_pat_" autocomplete="off" />
                <small>Leave blank when editing to keep the stored token.</small>
              </label>

              <div class="detected-token-row">
                <span>Detected token</span>
                <strong>{{ selectedGitAccount ? tokenKindLabel(selectedGitAccount.token_kind) : 'After Test connection' }}</strong>
                <small>{{ selectedGitAccount?.scopes?.length ? selectedGitAccount.scopes.join(', ') : 'Permissions are read from GitHub when available.' }}</small>
              </div>
              <p v-if="selectedGitAccount?.last_error" class="dialog-error">{{ selectedGitAccount.last_error }}</p>

              <div class="agent-runtime-strip">
                <span>Agent runtime</span>
                <strong>Stored token -> Git MCP + GitHub MCP</strong>
                <small>Token stays in server-managed MCP processes, not in the agent container.</small>
              </div>

              <div class="settings-actions">
                <div class="settings-actions-left">
                  <button class="ghost-button" type="button" :disabled="gitAccountsState.saving || !gitForm.id" @click="resetGitForm">New</button>
                  <button class="ghost-button" type="button" :disabled="gitAccountsState.verifying || !gitForm.id" @click="$emit('verify-git-account', gitForm.id)">
                    <span v-if="gitAccountsState.verifying" class="spinner-sm"></span>
                    <template v-else>Test connection</template>
                  </button>
                </div>
                <button class="ghost-button" type="button" :disabled="!gitForm.id || selectedGitAccount?.is_default" @click="$emit('set-default-git-account', gitForm.id)">Set Default</button>
                <button class="primary-button" type="submit" :disabled="gitAccountsState.saving || !canSaveGitAccount">
                  <span v-if="gitAccountsState.saving" class="spinner-sm"></span>
                  <template v-else>Save Account</template>
                </button>
              </div>
              <button v-if="gitForm.id" class="danger-button git-delete-button" type="button" :disabled="gitAccountsState.deleting" @click="$emit('delete-git-account', gitForm.id)">Delete Account</button>
            </form>
          </div>
        </section>

        <section v-else class="settings-section-panel mcp-settings">
          <div class="settings-section-header">
            <div>
              <h2>MCP Servers</h2>
              <p>Stdio servers run inside agent containers; Streamable HTTP connects from Mai.</p>
            </div>
            <div class="settings-actions-left">
              <button class="ghost-button" type="button" :disabled="mcpServersState.loading" @click="$emit('reload-mcp')">Reload</button>
              <button class="primary-button" type="button" :disabled="mcpServersState.loading" @click="$emit('open-mcp')">
                Configure MCP
              </button>
            </div>
          </div>

          <div class="mcp-summary-grid">
            <div class="settings-summary-item">
              <span>Servers</span>
              <strong>{{ mcpServerCount }}</strong>
              <small>{{ mcpEnabledCount }} enabled</small>
            </div>
            <div class="settings-summary-item">
              <span>Transports</span>
              <strong>{{ mcpTransportLabel }}</strong>
              <small>stdio and Streamable HTTP supported.</small>
            </div>
          </div>
        </section>
      </div>
    </div>
  </section>
</template>

<script setup>
import { computed, reactive, ref, watch } from 'vue'
import ModelSelector from './ModelSelector.vue'
import { defaultReasoningEffort, reasoningLabel } from '../utils/reasoning'

const ROLE_DEFINITIONS = [
  {
    key: 'planner',
    title: 'Planner',
    initial: 'P',
    description: 'Decomposes tasks, plans steps, and calls out dependencies.'
  },
  {
    key: 'explorer',
    title: 'Explorer',
    initial: 'X',
    description: 'Explores code, docs, and web context before implementation.'
  },
  {
    key: 'executor',
    title: 'Executor',
    initial: 'E',
    description: 'Writes code, runs commands, and completes implementation work.'
  },
  {
    key: 'reviewer',
    title: 'Reviewer',
    initial: 'R',
    description: 'Reviews changes, finds regressions, and highlights test gaps.'
  }
]

const SETTINGS_SECTIONS = [
  { key: 'roles', title: 'Role Models', icon: 'R' },
  { key: 'skills', title: 'Skills', icon: 'S' },
  { key: 'git-accounts', title: 'Git Accounts', icon: 'G' },
  { key: 'mcp', title: 'MCP Servers', icon: 'M' }
]

const props = defineProps({
  providers: { type: Array, default: () => [] },
  state: { type: Object, required: true },
  skillsState: { type: Object, required: true },
  loading: { type: Boolean, default: false },
  saving: { type: Boolean, default: false },
  skillsSaving: { type: Boolean, default: false },
  skillsError: { type: String, default: '' },
  mcpServersState: { type: Object, required: true },
  mcpSaving: { type: Boolean, default: false },
  gitAccountsState: { type: Object, default: () => ({ accounts: [] }) },
  initialSection: { type: String, default: 'roles' }
})

const emit = defineEmits([
  'save',
  'reload',
  'open-providers',
  'reload-skills',
  'save-skills',
  'reload-mcp',
  'open-mcp',
  'save-git-account',
  'verify-git-account',
  'delete-git-account',
  'set-default-git-account'
])

const activeSettingsSection = ref('roles')
const selectedGitAccountId = ref('')
const DEFAULT_GIT_ACCOUNT_LABEL = 'Personal'
const gitForm = reactive({
  id: '',
  label: DEFAULT_GIT_ACCOUNT_LABEL,
  token: ''
})

const error = ref('')
const forms = reactive({
  planner: emptyPreference(),
  explorer: emptyPreference(),
  executor: emptyPreference(),
  reviewer: emptyPreference()
})
const skillOverrides = reactive({})

const providerSeedLabel = computed(() => {
  const provider = props.providers[0]
  if (!provider) return 'No provider'
  const model = provider.default_model || provider.models?.[0]?.id || 'No model'
  return `${provider.name || provider.id} / ${model}`
})

const configuredCount = computed(() =>
  ROLE_DEFINITIONS.filter((role) => savedPreference(role.key)).length
)

const canSave = computed(() =>
  ROLE_DEFINITIONS.every((role) => forms[role.key].provider_id && forms[role.key].model)
)

const enabledSkillCount = computed(() =>
  (props.skillsState.skills || []).filter((skill) => skillEnabled(skill)).length
)

const disabledSkillCount = computed(() =>
  Math.max((props.skillsState.skills || []).length - enabledSkillCount.value, 0)
)

const providersWithKeysCount = computed(() =>
  props.providers.filter((provider) => provider.has_api_key).length
)

const reasoningConfigured = computed(() =>
  ROLE_DEFINITIONS.every((role) => forms[role.key].reasoning_effort || forms[role.key].model)
)

const mcpServers = computed(() => Object.values(props.mcpServersState.servers || {}))
const mcpServerCount = computed(() => mcpServers.value.length)
const mcpEnabledCount = computed(() => mcpServers.value.filter((server) => server.enabled !== false).length)
const mcpTransportLabel = computed(() => {
  const transports = new Set(mcpServers.value.map((server) => server.transport || 'stdio'))
  if (!transports.size) return 'None'
  return Array.from(transports).join(' · ')
})
const gitAccounts = computed(() => props.gitAccountsState.accounts || [])
const gitAccountCount = computed(() => gitAccounts.value.length)
const defaultGitAccount = computed(() => gitAccounts.value.find((account) => account.is_default) || null)
const selectedGitAccount = computed(() => gitAccounts.value.find((account) => account.id === selectedGitAccountId.value) || null)
const canSaveGitAccount = computed(() => {
  return Boolean(gitForm.label.trim() && (gitForm.id || gitForm.token.trim()))
})

watch(
  () => [
    props.state.planner,
    props.state.explorer,
    props.state.executor,
    props.state.reviewer,
    props.providers
  ],
  resetFromState,
  { immediate: true }
)

watch(
  () => props.initialSection,
  (section) => {
    if (SETTINGS_SECTIONS.some((item) => item.key === section)) {
      activeSettingsSection.value = section
    }
  },
  { immediate: true }
)

watch(
  () => props.skillsState.skills,
  () => {
    for (const key of Object.keys(skillOverrides)) delete skillOverrides[key]
  }
)

watch(gitAccounts, syncSelectedGitAccount, { immediate: true })

function emptyPreference() {
  return {
    provider_id: '',
    model: '',
    reasoning_effort: ''
  }
}

function savedPreference(role) {
  return props.state?.[role] || null
}

function effectivePreference(role) {
  return props.state?.[`effective_${role}`] || null
}

function resetFromState() {
  for (const role of ROLE_DEFINITIONS) {
    const configured = savedPreference(role.key)
    const fallback = defaultPreference(configured)
    forms[role.key].provider_id = configured?.provider_id || fallback.provider_id
    forms[role.key].model = configured?.model || fallback.model
    forms[role.key].reasoning_effort = configured?.reasoning_effort || fallback.reasoning_effort
  }
  error.value = ''
}

function defaultPreference(configured) {
  const provider = configured
    ? props.providers.find((item) => item.id === configured.provider_id) || props.providers[0]
    : props.providers[0]
  const modelId = configured?.model || provider?.default_model || provider?.models?.[0]?.id || ''
  const model = provider?.models?.find((item) => item.id === modelId)
  return {
    provider_id: provider?.id || '',
    model: modelId,
    reasoning_effort: defaultReasoningEffort(provider, model)
  }
}

function effectiveLabel(role) {
  const staged = forms[role]
  const effective = effectivePreference(role)
  if (matchesEffective(staged, effective)) return modelLabel(effective)
  return stagedLabel(staged)
}

function matchesEffective(staged, effective) {
  if (!staged || !effective) return false
  return staged.provider_id === effective.provider_id &&
    staged.model === effective.model &&
    (staged.reasoning_effort || '') === (effective.reasoning_effort || '')
}

function modelLabel(preference) {
  if (!preference) return 'Not saved yet'
  return [
    preference.provider_name || providerName(preference.provider_id),
    preference.model_name || modelName(preference.provider_id, preference.model),
    reasoningLabel(preference.reasoning_effort)
  ].filter(Boolean).join(' · ')
}

function stagedLabel(preference) {
  if (!preference?.provider_id || !preference?.model) return 'Missing provider or model'
  return [
    providerName(preference.provider_id),
    modelName(preference.provider_id, preference.model),
    reasoningLabel(preference.reasoning_effort)
  ].filter(Boolean).join(' · ')
}

function providerName(providerId) {
  return props.providers.find((item) => item.id === providerId)?.name || providerId
}

function modelName(providerId, modelId) {
  const provider = props.providers.find((item) => item.id === providerId)
  return provider?.models?.find((item) => item.id === modelId)?.name || modelId
}

function save() {
  error.value = ''
  if (!canSave.value) {
    error.value = 'Provider and model are required for every role.'
    return
  }
  emit('save', {
    planner: preferencePayload(forms.planner),
    explorer: preferencePayload(forms.explorer),
    executor: preferencePayload(forms.executor),
    reviewer: preferencePayload(forms.reviewer)
  })
}

function preferencePayload(preference) {
  return {
    provider_id: preference.provider_id,
    model: preference.model,
    reasoning_effort: preference.reasoning_effort || null
  }
}

function sectionStatus(section) {
  if (section === 'roles') return `${configuredCount.value}/${ROLE_DEFINITIONS.length} configured`
  if (section === 'skills') return `${enabledSkillCount.value} enabled`
  if (section === 'git-accounts') return gitAccountCount.value ? `${gitAccountCount.value} connected` : 'No accounts'
  if (section === 'mcp') return `${mcpEnabledCount.value} active`
  return ''
}

function skillEnabled(skill) {
  return Object.prototype.hasOwnProperty.call(skillOverrides, skill.path)
    ? skillOverrides[skill.path]
    : skill.enabled
}

function toggleSkill(skill) {
  skillOverrides[skill.path] = !skillEnabled(skill)
  skill.enabled = skillOverrides[skill.path]
}

function saveSkills() {
  const config = props.skillsState.skills.map((skill) => ({
    path: skill.path,
    enabled: skillEnabled(skill)
  }))
  emit('save-skills', config)
}

function syncSelectedGitAccount() {
  if (selectedGitAccountId.value && gitAccounts.value.some((account) => account.id === selectedGitAccountId.value)) return
  const next = defaultGitAccount.value || gitAccounts.value[0]
  if (next) selectGitAccount(next)
  else resetGitForm()
}

function selectGitAccount(account) {
  selectedGitAccountId.value = account.id
  gitForm.id = account.id
  gitForm.label = account.label || ''
  gitForm.token = ''
}

function resetGitForm() {
  selectedGitAccountId.value = ''
  gitForm.id = ''
  gitForm.label = DEFAULT_GIT_ACCOUNT_LABEL
  gitForm.token = ''
}

function saveGitAccountForm() {
  const token = gitForm.token.trim()
  const clearSavedToken = () => {
    if (gitForm.token.trim() === token) gitForm.token = ''
  }
  emit(
    'save-git-account',
    {
      id: gitForm.id || null,
      provider: 'github',
      label: gitForm.label.trim(),
      token: token || null,
      is_default: !gitAccountCount.value
    },
    clearSavedToken
  )
}

function accountDisplay(account) {
  return account?.login ? `@${account.login}` : (account?.provider || 'github')
}

function tokenKindLabel(value) {
  if (value === 'classic') return 'Classic token'
  if (value === 'fine_grained_pat') return 'Fine-grained PAT'
  return 'Token type pending'
}

function gitAccountStatusLabel(value) {
  if (value === 'verified') return 'Verified'
  if (value === 'verifying') return 'Verifying'
  if (value === 'failed') return 'Failed'
  return 'Unverified'
}

function gitAccountStatusClass(value) {
  if (value === 'verified') return 'ready'
  if (value === 'failed') return 'danger'
  return ''
}
</script>
