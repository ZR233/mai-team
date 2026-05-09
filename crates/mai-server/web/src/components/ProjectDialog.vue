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
              @click="dialog.mode = mode.id"
            >
              <strong>{{ mode.label }}</strong>
              <span>{{ mode.description }}</span>
            </button>
          </div>

          <div v-if="!hasAccounts" class="project-github-empty">
            <strong>No Git accounts configured</strong>
            <span>Add a GitHub token before creating projects.</span>
            <button class="primary-button" type="button" @click="$emit('configure-git-accounts')">Open Git Accounts</button>
          </div>

          <div class="project-create-form">
            <label class="project-field-row">
              <span>Project Name</span>
              <input v-model.trim="dialog.form.name" :placeholder="projectNamePlaceholder" />
            </label>

            <div class="project-field-row">
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
                  :disabled="!dialog.form.git_account_id || dialog.loadingRepositories"
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

            <div class="project-field-row two-col">
              <span></span>
              <div class="project-two-controls">
                <label>
                  <span>Branch</span>
                  <input v-model.trim="dialog.form.branch" :disabled="!selectedRepository" placeholder="main" />
                </label>
                <label>
                  <span>Project Path</span>
                  <input v-model.trim="dialog.form.project_path" placeholder="/" />
                </label>
              </div>
            </div>

            <div class="project-field-row align-start">
              <span>Docker Image</span>
              <div class="project-picker">
                <input v-model.trim="dialog.runtime.docker_image" placeholder="ghcr.io/rcore-os/tgoskits-container:latest" />
                <div class="project-picker-list compact-list" aria-label="Docker image suggestions">
                  <button
                    v-for="image in dockerImages"
                    :key="image"
                    class="project-picker-option"
                    type="button"
                    :class="{ active: dialog.runtime.docker_image === image }"
                    @click="dialog.runtime.docker_image = image"
                  >
                    <span>{{ image }}</span>
                    <strong v-if="dialog.runtime.docker_image === image">Selected</strong>
                  </button>
                </div>
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
                <small>{{ hasAccounts ? accountCountLabel : 'Add a Git account' }}</small>
              </div>
            </li>
            <li :class="{ done: Boolean(selectedAccount) }">
              <span></span>
              <div>
                <strong>Select account</strong>
                <small>{{ selectedAccount ? accountLabel(selectedAccount) : 'No account selected' }}</small>
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
          <p class="project-create-note">Repository access is verified with the selected Git account token.</p>
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
  'refresh-repositories'
])

const modes = [
  { id: 'git_account', label: 'Git Account', description: 'Select account and repository' },
  { id: 'local_git', label: 'Local Git', description: 'Coming soon', disabled: true },
  { id: 'upload', label: 'Upload', description: 'Coming soon', disabled: true }
]

const dockerImages = [
  'ghcr.io/rcore-os/tgoskits-container:latest',
  'ghcr.io/rcore-os/tgoskits-container:v1.2.0',
  'ghcr.io/rcore-os/tgoskits-container:v1.1.0'
]

const hasAccounts = computed(() => props.dialog.gitAccounts.length > 0)
const selectedAccount = computed(() => props.dialog.gitAccounts.find((account) => account.id === props.dialog.form.git_account_id) || null)
const accountCountLabel = computed(() => `${props.dialog.gitAccounts.length} account${props.dialog.gitAccounts.length === 1 ? '' : 's'}`)

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
  return hasAccounts.value
    && props.dialog.form.git_account_id
    && props.dialog.form.repository_full_name
    && runtimeReady.value
    && !props.dialog.submitting
})

const projectNamePlaceholder = computed(() => selectedRepository.value ? repositoryLabel(selectedRepository.value) : 'owner/repository')

const repositoryPlaceholder = computed(() => {
  if (props.dialog.loadingRepositories) return 'Loading repositories'
  if (!props.dialog.form.git_account_id) return 'Select account first'
  if (!props.dialog.repositories.length) return 'No repositories loaded'
  return 'No repositories match'
})

const setupSummary = computed(() => {
  if (!hasAccounts.value) return 'Git account required'
  if (!selectedAccount.value) return 'Select account'
  if (!selectedRepository.value) return 'Choose repository'
  return 'Ready to create'
})

const runtimeLabel = computed(() => {
  if (!runtimeReady.value) return 'Invalid image ID'
  return props.dialog.runtime.docker_image.trim() || 'Default image'
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

function onAccountChanged() {
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
}

function accountLabel(account) {
  const login = account.login ? `@${account.login}` : account.provider || 'github'
  return `${account.label || 'GitHub'} · ${login}`
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
