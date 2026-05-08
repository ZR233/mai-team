<template>
  <div v-if="dialog.open" class="modal-backdrop" @mousedown.self="onBackdropDown" @mouseup.self="onBackdropUp($event, () => $emit('close'))">
    <form class="modal wide" @submit.prevent="$emit('create')">
      <div class="modal-title-row">
        <h2>New Project</h2>
        <button class="icon-button" type="button" @click="$emit('close')">Close</button>
      </div>

      <div v-if="!githubConfigured" class="project-github-empty">
        <strong>GitHub App is not configured</strong>
        <span>Create the Mai GitHub App first, then install it on repositories you want agents to work with.</span>
        <button class="primary-button" type="button" @click="$emit('configure-github-app')">Configure GitHub App</button>
      </div>

      <div v-else-if="!dialog.installations.length && !dialog.loadingInstallations" class="project-github-empty">
        <strong>No installations found</strong>
        <span>Install the GitHub App on selected repositories, then refresh installations.</span>
        <div class="project-empty-actions">
          <button v-if="githubState.install_url" class="primary-button" type="button" @click="$emit('install-github-app')">Install App</button>
          <button class="ghost-button" type="button" @click="$emit('refresh-installations')">Refresh Installations</button>
        </div>
      </div>

      <div class="form-grid">
        <label class="span-2">
          <span>Project Name</span>
          <input v-model.trim="dialog.form.name" placeholder="mai-team frontend" required />
        </label>

        <label>
          <span>GitHub Installation</span>
          <select v-model="dialog.form.installation_id" required @change="onInstallationChanged">
            <option value="" disabled>Select installation</option>
            <option
              v-for="installation in dialog.installations"
              :key="installation.id"
              :value="String(installation.id)"
            >
              {{ installationLabel(installation) }}
            </option>
          </select>
        </label>
        <div class="project-dialog-action">
          <button class="ghost-button" type="button" :disabled="dialog.loadingInstallations" @click="$emit('refresh-installations')">
            <span v-if="dialog.loadingInstallations" class="spinner-sm"></span>
            <template v-else>Refresh Installations</template>
          </button>
        </div>

        <label>
          <span>Repository</span>
          <select v-model="dialog.form.repository_id" required @change="onRepositoryChanged">
            <option value="" disabled>Select repository</option>
            <option
              v-for="repository in dialog.repositories"
              :key="repository.id || repository.full_name"
              :value="String(repository.id || repository.full_name)"
            >
              {{ repositoryLabel(repository) }}
            </option>
          </select>
        </label>
        <div class="project-dialog-action">
          <button
            class="ghost-button"
            type="button"
            :disabled="!dialog.form.installation_id || dialog.loadingRepositories"
            @click="$emit('refresh-repositories')"
          >
            <span v-if="dialog.loadingRepositories" class="spinner-sm"></span>
            <template v-else>Refresh Repositories</template>
          </button>
        </div>

        <label class="span-2">
          <span>Docker Image</span>
          <input v-model.trim="dialog.form.docker_image" placeholder="ghcr.io/rcore-os/tgoskits-container:latest" />
        </label>
      </div>

      <div class="permission-summary" aria-label="Requested permissions">
        <span>Contents write</span>
        <span>Pull requests write</span>
        <span>Issues write</span>
      </div>

      <p v-if="dialog.error" class="dialog-error">{{ dialog.error }}</p>
      <div class="modal-actions">
        <button class="ghost-button" type="button" @click="$emit('close')">Cancel</button>
        <button class="primary-button" type="submit" :disabled="dialog.submitting">
          <span v-if="dialog.submitting" class="spinner-sm"></span>
          <template v-else>Create</template>
        </button>
      </div>
    </form>
  </div>
</template>

<script setup>
import { computed } from 'vue'

const props = defineProps({
  dialog: { type: Object, required: true },
  githubState: { type: Object, default: () => ({}) }
})

const emit = defineEmits([
  'close',
  'create',
  'configure-github-app',
  'install-github-app',
  'refresh-installations',
  'refresh-repositories'
])

const githubConfigured = computed(() => Boolean(props.githubState.app_id && props.githubState.has_private_key))

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

function onInstallationChanged() {
  props.dialog.form.repository_id = ''
  props.dialog.form.owner = ''
  props.dialog.form.repo = ''
  emit('refresh-repositories')
}

function onRepositoryChanged() {
  const selected = props.dialog.repositories.find((repository) => {
    return String(repository.id || repository.full_name) === String(props.dialog.form.repository_id)
  })
  if (!selected) return
  const [owner, repo] = parseRepositoryName(selected)
  props.dialog.form.owner = owner
  props.dialog.form.repo = repo
}

function installationLabel(installation) {
  const account = installation.account?.login
    || installation.account_login
    || installation.account
    || installation.installation_account
    || 'GitHub App'
  return `${account} · ${installation.id}`
}

function repositoryLabel(repository) {
  return repository.full_name || [repository.owner?.login || repository.owner, repository.name].filter(Boolean).join('/') || repository.id
}

function parseRepositoryName(repository) {
  const fullName = repository.full_name || ''
  if (fullName.includes('/')) return fullName.split('/', 2)
  return [repository.owner?.login || repository.owner || '', repository.name || repository.repo || '']
}
</script>
