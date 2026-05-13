<template>
  <div v-if="dialog.open" class="modal-backdrop" @mousedown.self="onBackdropDown" @mouseup.self="onBackdropUp($event, () => $emit('close'))">
    <form class="modal project-create-modal" @submit.prevent="$emit('create')">
      <div class="modal-title-row">
        <div>
          <h2>New Project</h2>
          <p>Create a maintained project from a verified GitHub account.</p>
        </div>
        <button class="icon-button" type="button" @click="$emit('close')">Close</button>
      </div>

      <div class="project-create-layout">
        <section class="project-create-main">
          <div class="project-mode-grid" aria-label="Project creation modes">
            <button
              v-for="mode in modes"
              :key="mode.id"
              class="project-mode-card"
              type="button"
              :class="{ active: dialog.mode === mode.id, disabled: mode.disabled }"
              :disabled="mode.disabled"
              @click="selectMode(mode.id)"
            >
              <strong>{{ mode.label }}</strong>
              <span>{{ mode.description }}</span>
            </button>
          </div>

          <div v-if="dialog.mode === 'git_account' && !hasAccounts" class="project-github-empty">
            <strong>No Git accounts configured</strong>
            <span>Add a GitHub token before creating projects.</span>
            <button class="primary-button" type="button" @click="$emit('configure-git-accounts')">Open Git Accounts</button>
          </div>

          <div v-else-if="dialog.mode === 'github_app' && !githubAppReady" class="project-github-empty">
            <strong>GitHub App relay unavailable</strong>
            <span>{{ githubAppStatus }}</span>
            <button class="ghost-button" type="button" :disabled="dialog.loadingInstallations" @click="$emit('refresh-installations')">Refresh</button>
          </div>

          <div class="project-create-form">
            <label class="project-field-row">
              <span>Project Name</span>
              <input v-model.trim="dialog.form.name" :placeholder="projectNamePlaceholder" />
            </label>

            <div v-if="dialog.mode === 'git_account'" class="project-field-row">
              <span>Git Account</span>
              <div class="project-control-with-action">
                <select v-model="dialog.form.git_account_id" :disabled="!hasAccounts" required @change="onAccountChanged">
                  <option value="" disabled>Select account</option>
                  <option v-for="account in dialog.gitAccounts" :key="account.id" :value="account.id">
                    {{ accountLabel(account) }}
                  </option>
                </select>
                <button class="ghost-button" type="button" :disabled="dialog.loadingRepositories || !dialog.form.git_account_id" @click="$emit('refresh-repositories')">
                  <span v-if="dialog.loadingRepositories" class="spinner-sm"></span>
                  <template v-else>Load Repos</template>
                </button>
              </div>
            </div>

            <div v-else class="project-field-row">
              <span>GitHub App</span>
              <div class="project-control-with-action">
                <select v-model="dialog.form.installation_id" :disabled="!githubAppReady || dialog.loadingInstallations" required @change="onInstallationChanged">
                  <option value="" disabled>{{ installationSelectPlaceholder }}</option>
                  <option v-for="installation in dialog.installations" :key="installation.id" :value="String(installation.id)">
                    {{ installationLabel(installation) }}
                  </option>
                </select>
                <button class="ghost-button" type="button" :disabled="!canInstallGithubApp || dialog.installingGithubApp" @click="$emit('install-github-app')">
                  <span v-if="dialog.installingGithubApp" class="spinner-sm"></span>
                  <template v-else>Install</template>
                </button>
                <button class="ghost-button" type="button" :disabled="!githubAppReady || dialog.loadingInstallations" @click="$emit('refresh-installations')">
                  <span v-if="dialog.loadingInstallations" class="spinner-sm"></span>
                  <template v-else>Refresh</template>
                </button>
              </div>
            </div>

            <div class="project-field-row">
              <span>Agent Tools</span>
              <div class="permission-summary compact" aria-label="Agent tools">
                <span>Git MCP</span>
                <span>GitHub MCP</span>
                <span>Same token</span>
                <span>Server-side</span>
              </div>
            </div>

            <div class="project-field-row align-start">
              <span>Repository</span>
              <div class="project-picker">
                <input
                  v-model.trim="dialog.repository.query"
                  :disabled="!canLoadRepositories || dialog.loadingRepositories"
                  placeholder="Search repositories"
                />
                <div class="project-picker-list" aria-label="Repositories">
                  <button
                    v-for="repository in visibleRepositories"
                    :key="repository.full_name || repository.id"
                    class="project-picker-option"
                    type="button"
                    :class="{ active: isSelectedRepository(repository) }"
                    @click="selectRepository(repository)"
                  >
                    <span>{{ repositoryLabel(repository) }}</span>
                    <strong v-if="isSelectedRepository(repository)">Selected</strong>
                  </button>
                  <div v-if="!visibleRepositories.length" class="project-picker-empty">
                    {{ repositoryPlaceholder }}
                  </div>
                </div>
              </div>
            </div>

            <label class="project-field-row">
              <span>Branch</span>
              <input v-model.trim="dialog.form.branch" :disabled="!selectedRepository" placeholder="main" />
            </label>

            <div class="project-field-row align-start">
              <span>Docker Image</span>
              <div class="project-runtime-picker">
                <input v-model.trim="dialog.runtime.docker_image" :placeholder="runtimePlaceholder" />
                <div class="project-control-with-action">
                  <select
                    v-model="dialog.runtime.package_image"
                    :disabled="dialog.runtime.loadingPackages || !dialog.runtime.packages.length"
                    @change="selectPackageImage"
                  >
                    <option value="" disabled>{{ packageSelectPlaceholder }}</option>
                    <option v-for="imagePackage in dialog.runtime.packages" :key="imagePackage.image" :value="imagePackage.image">
                      {{ imagePackage.image }}
                    </option>
                  </select>
                  <button
                    class="ghost-button"
                    type="button"
                    :disabled="dialog.runtime.loadingPackages || !selectedRepository"
                    @click="$emit('load-repository-packages')"
                  >
                    <span v-if="dialog.runtime.loadingPackages" class="spinner-sm"></span>
                    <template v-else>Load Images</template>
                  </button>
                </div>
                <small v-if="dialog.runtime.packageWarning" class="project-runtime-note danger">{{ dialog.runtime.packageWarning }}</small>
                <small v-else class="project-runtime-note">{{ runtimeHelpText }}</small>
              </div>
            </div>
          </div>
        </section>

        <aside class="project-create-side" aria-label="Project setup status">
          <div class="project-side-head">
            <strong>Setup</strong>
            <span>{{ setupSummary }}</span>
          </div>
          <ol class="project-checklist">
            <li :class="{ done: hasAccounts }">
              <span></span>
              <div>
                <strong>Connect GitHub</strong>
                <small>{{ connectStepLabel }}</small>
              </div>
            </li>
            <li :class="{ done: Boolean(selectedAccount) }">
              <span></span>
              <div>
                <strong>{{ dialog.mode === 'github_app' ? 'Select installation' : 'Select account' }}</strong>
                <small>{{ selectedSourceLabel }}</small>
              </div>
            </li>
            <li :class="{ done: Boolean(selectedRepository) }">
              <span></span>
              <div>
                <strong>Choose repository</strong>
                <small>{{ selectedRepository ? repositoryLabel(selectedRepository) : 'No repository selected' }}</small>
              </div>
            </li>
            <li :class="{ done: runtimeReady }">
              <span></span>
              <div>
                <strong>Select runtime</strong>
                <small>{{ runtimeLabel }}</small>
              </div>
            </li>
          </ol>
          <p class="project-create-note">{{ accessNote }}</p>
        </aside>
      </div>

      <p v-if="dialog.error" class="dialog-error">{{ dialog.error }}</p>
      <div class="modal-actions">
        <button class="ghost-button" type="button" @click="$emit('close')">Cancel</button>
        <button class="primary-button" type="submit" :disabled="!canCreate">
          <span v-if="dialog.submitting" class="spinner-sm"></span>
          <template v-else>Create Project</template>
        </button>
      </div>
    </form>
  </div>
</template>

<script setup>
import { computed } from 'vue'

const props = defineProps({
  dialog: { type: Object, required: true }
})

const emit = defineEmits([
  'close',
  'create',
  'configure-git-accounts',
  'install-github-app',
  'refresh-installations',
  'refresh-repositories',
  'load-repository-packages'
])

const modes = [
  { id: 'git_account', label: 'Git Account', description: 'Select account and repository' },
  { id: 'github_app', label: 'GitHub App', description: 'Install app and select repository' },
  { id: 'local_git', label: 'Local Git', description: 'Coming soon', disabled: true }
]

const hasAccounts = computed(() => props.dialog.gitAccounts.length > 0)
const selectedAccount = computed(() => props.dialog.gitAccounts.find((account) => account.id === props.dialog.form.git_account_id) || null)
const selectedInstallation = computed(() => props.dialog.installations.find((installation) => String(installation.id) === String(props.dialog.form.installation_id)) || null)
const accountCountLabel = computed(() => `${props.dialog.gitAccounts.length} account${props.dialog.gitAccounts.length === 1 ? '' : 's'}`)
const installationCountLabel = computed(() => `${props.dialog.installations.length} installation${props.dialog.installations.length === 1 ? '' : 's'}`)
const relayConnected = computed(() => props.dialog.relay?.enabled && props.dialog.relay?.connected)
const githubAppReady = computed(() => relayConnected.value && Boolean(props.dialog.githubApp?.app_slug || props.dialog.githubApp?.install_url))
const canInstallGithubApp = computed(() => relayConnected.value && Boolean(props.dialog.githubApp?.app_slug || props.dialog.githubApp?.install_url))
const canLoadRepositories = computed(() => props.dialog.mode === 'github_app' ? Boolean(props.dialog.form.installation_id) : Boolean(props.dialog.form.git_account_id))
const sourceSelected = computed(() => props.dialog.mode === 'github_app' ? Boolean(selectedInstallation.value) : Boolean(selectedAccount.value))

const filteredRepositories = computed(() => {
  const query = props.dialog.repository.query.trim().toLowerCase()
  if (!query) return props.dialog.repositories
  return props.dialog.repositories.filter((repository) => repositoryLabel(repository).toLowerCase().includes(query))
})

const visibleRepositories = computed(() => filteredRepositories.value.slice(0, 6))

const selectedRepository = computed(() => {
  return props.dialog.repositories.find((repository) => {
    return repository.full_name === props.dialog.form.repository_full_name
  }) || null
})

const runtimeReady = computed(() => {
  const image = props.dialog.runtime.docker_image.trim()
  return !image || isValidDockerImage(image)
})

const canCreate = computed(() => {
  return sourceSelected.value
    && props.dialog.form.repository_full_name
    && runtimeReady.value
    && !props.dialog.submitting
})

const projectNamePlaceholder = computed(() => selectedRepository.value ? repositoryLabel(selectedRepository.value) : 'owner/repository')

const repositoryPlaceholder = computed(() => {
  if (props.dialog.loadingRepositories) return 'Loading repositories'
  if (!canLoadRepositories.value) return props.dialog.mode === 'github_app' ? 'Select installation first' : 'Select account first'
  if (!props.dialog.repositories.length) return 'No repositories loaded'
  return 'No repositories match'
})

const setupSummary = computed(() => {
  if (props.dialog.mode === 'github_app' && !githubAppReady.value) return 'GitHub App relay required'
  if (props.dialog.mode === 'git_account' && !hasAccounts.value) return 'Git account required'
  if (!sourceSelected.value) return props.dialog.mode === 'github_app' ? 'Select installation' : 'Select account'
  if (!selectedRepository.value) return 'Choose repository'
  return 'Ready to create'
})

const githubAppStatus = computed(() => {
  if (!props.dialog.relay?.enabled) return 'Enable mai-server relay mode before using GitHub App projects.'
  if (!props.dialog.relay?.connected) return props.dialog.relay?.message || 'mai-server is not connected to mai-relay.'
  if (!props.dialog.githubApp?.app_slug && !props.dialog.githubApp?.install_url) return 'Configure GitHub App environment variables on mai-relay.'
  return 'GitHub App relay is ready.'
})

const installationSelectPlaceholder = computed(() => {
  if (props.dialog.loadingInstallations) return 'Loading installations'
  if (!githubAppReady.value) return 'GitHub App unavailable'
  if (!props.dialog.installations.length) return 'Install GitHub App first'
  return 'Select installation'
})

const connectStepLabel = computed(() => {
  if (props.dialog.mode === 'github_app') {
    if (!githubAppReady.value) return githubAppStatus.value
    return props.dialog.installations.length ? installationCountLabel.value : 'Install GitHub App'
  }
  return hasAccounts.value ? accountCountLabel.value : 'Add a Git account'
})

const selectedSourceLabel = computed(() => {
  if (props.dialog.mode === 'github_app') return selectedInstallation.value ? installationLabel(selectedInstallation.value) : 'No installation selected'
  return selectedAccount.value ? accountLabel(selectedAccount.value) : 'No account selected'
})

const accessNote = computed(() => props.dialog.mode === 'github_app'
  ? 'Repository access is verified with a short-lived GitHub App installation token.'
  : 'Repository access is verified with the selected Git account token.')

const runtimeLabel = computed(() => {
  if (!runtimeReady.value) return 'Invalid image ID'
  return props.dialog.runtime.docker_image.trim() || 'Default image'
})

const runtimePlaceholder = computed(() => props.dialog.runtime.default_docker_image || 'Docker image')

const packageSelectPlaceholder = computed(() => {
  if (!selectedRepository.value) return 'Select repository first'
  if (props.dialog.runtime.loadingPackages) return 'Loading repository images'
  if (!props.dialog.runtime.packages.length) return 'No repository images'
  return 'Select repository image'
})

const runtimeHelpText = computed(() => {
  if (props.dialog.runtime.packages.length) return 'Choose a repository image or type any Docker image.'
  if (selectedRepository.value) return 'No repository images loaded. You can still type any Docker image.'
  return 'Select a repository to load related images, or type any Docker image.'
})

let backdropDown = false

function onBackdropDown() {
  backdropDown = true
}

function onBackdropUp(_event, callback) {
  if (backdropDown) {
    backdropDown = false
    callback()
  }
}

function selectMode(mode) {
  props.dialog.mode = mode
  props.dialog.error = ''
  props.dialog.form.repository_full_name = ''
  props.dialog.form.branch = ''
  props.dialog.repository.query = ''
  emit('refresh-repositories')
}

function onAccountChanged() {
  props.dialog.form.repository_full_name = ''
  props.dialog.form.branch = ''
  props.dialog.repository.query = ''
  emit('refresh-repositories')
}

function onInstallationChanged() {
  props.dialog.form.repository_full_name = ''
  props.dialog.form.branch = ''
  props.dialog.repository.query = ''
  emit('refresh-repositories')
}

function selectRepository(repository) {
  props.dialog.form.repository_full_name = repository.full_name
  props.dialog.form.branch = repository.default_branch || 'main'
  if (!props.dialog.form.name.trim()) {
    props.dialog.form.name = repositoryLabel(repository)
  }
  emit('load-repository-packages')
}

function selectPackageImage() {
  if (props.dialog.runtime.package_image) {
    props.dialog.runtime.docker_image = props.dialog.runtime.package_image
  }
}

function accountLabel(account) {
  const login = account.login ? `@${account.login}` : account.provider || 'github'
  return `${account.label || 'GitHub'} · ${login}`
}

function installationLabel(installation) {
  const account = installation.account_login || installation.id
  const type = installation.account_type || 'GitHub'
  const selection = installation.repository_selection ? ` · ${installation.repository_selection}` : ''
  return `${account} · ${type}${selection}`
}

function repositoryLabel(repository) {
  return repository.full_name || [repository.owner?.login || repository.owner, repository.name].filter(Boolean).join('/') || repository.id
}

function isSelectedRepository(repository) {
  return repository.full_name === props.dialog.form.repository_full_name
}

function isValidDockerImage(value) {
  return /^[A-Za-z0-9][A-Za-z0-9._/:@-]{0,254}$/.test(value) && !/\s/.test(value)
}
</script>
