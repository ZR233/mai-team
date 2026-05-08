<template>
  <div
    v-if="open"
    class="modal-backdrop"
    @mousedown.self="onBackdropDown"
    @mouseup.self="onBackdropUp($event, close)"
  >
    <form class="modal wide" @submit.prevent="save">
      <div class="modal-title-row">
        <h2>MCP Servers</h2>
        <button class="icon-button" type="button" @click="close">Close</button>
      </div>

      <div class="mcp-server-list">
        <article v-for="server in servers" :key="server.id" class="mcp-server-row">
          <div class="mcp-server-head">
            <label>
              <span>Name</span>
              <input v-model.trim="server.name" placeholder="filesystem" />
            </label>
            <label>
              <span>Transport</span>
              <select v-model="server.transport">
                <option value="stdio">stdio</option>
                <option value="streamable_http">streamable_http</option>
              </select>
            </label>
            <button class="danger-button" type="button" @click="removeServer(server.id)">Remove</button>
          </div>

          <div class="form-grid">
            <label v-if="server.transport === 'stdio'" class="span-2">
              <span>Command</span>
              <input v-model.trim="server.command" placeholder="npx" />
            </label>
            <label v-if="server.transport === 'stdio'" class="span-2">
              <span>Args JSON</span>
              <textarea v-model="server.argsText" rows="3" placeholder='["@modelcontextprotocol/server-filesystem", "/workspace"]' />
            </label>
            <label v-if="server.transport === 'stdio'" class="span-2">
              <span>CWD</span>
              <input v-model.trim="server.cwd" placeholder="/workspace" />
            </label>

            <label v-if="server.transport === 'streamable_http'" class="span-2">
              <span>URL</span>
              <input v-model.trim="server.url" placeholder="https://example.com/mcp" />
            </label>
            <label v-if="server.transport === 'streamable_http'">
              <span>Bearer Token</span>
              <input v-model="server.bearer_token" type="password" placeholder="Leave blank to use env" />
            </label>
            <label v-if="server.transport === 'streamable_http'">
              <span>Bearer Token Env</span>
              <input v-model.trim="server.bearer_token_env" placeholder="MY_MCP_TOKEN" />
            </label>
            <label v-if="server.transport === 'streamable_http'" class="span-2">
              <span>Headers JSON</span>
              <textarea v-model="server.headersText" rows="3" placeholder='{"X-Workspace":"mai"}' />
            </label>

            <label>
              <span>Enabled</span>
              <select v-model="server.enabled">
                <option :value="true">Enabled</option>
                <option :value="false">Disabled</option>
              </select>
            </label>
            <label>
              <span>Required</span>
              <select v-model="server.required">
                <option :value="false">No</option>
                <option :value="true">Yes</option>
              </select>
            </label>
            <label>
              <span>Startup Timeout</span>
              <input v-model.number="server.startup_timeout_secs" type="number" min="0" placeholder="30" />
            </label>
            <label>
              <span>Tool Timeout</span>
              <input v-model.number="server.tool_timeout_secs" type="number" min="0" placeholder="120" />
            </label>
            <label class="span-2">
              <span>Env JSON</span>
              <textarea v-model="server.envText" rows="3" placeholder='{"KEY":"value"}' />
            </label>
            <label>
              <span>Enabled Tools JSON</span>
              <textarea v-model="server.enabledToolsText" rows="3" placeholder='["read_file"]' />
            </label>
            <label>
              <span>Disabled Tools JSON</span>
              <textarea v-model="server.disabledToolsText" rows="3" placeholder='["dangerous_tool"]' />
            </label>
          </div>
        </article>

        <div v-if="!servers.length" class="quiet-empty compact">
          <strong>No MCP servers configured</strong>
        </div>
      </div>

      <p v-if="error" class="dialog-error">{{ error }}</p>
      <div class="modal-actions">
        <button class="ghost-button" type="button" @click="addServer">Add Server</button>
        <button class="primary-button" type="submit" :disabled="saving">
          <span v-if="saving" class="spinner-sm"></span>
          <template v-else>Save MCP</template>
        </button>
      </div>
    </form>
  </div>
</template>

<script setup>
import { ref, watch } from 'vue'

const props = defineProps({
  open: { type: Boolean, default: false },
  serversState: { type: Object, required: true },
  saving: { type: Boolean, default: false }
})

const emit = defineEmits(['close', 'save'])

const servers = ref([])
const error = ref('')
let backdropDown = false

watch(
  () => [props.open, props.serversState.servers],
  () => {
    if (props.open) reset()
  },
  { immediate: true }
)

function reset() {
  servers.value = Object.entries(props.serversState.servers || {}).map(([name, config]) =>
    toForm(name, config)
  )
  error.value = ''
}

function toForm(name, config = {}) {
  return {
    id: randomId(),
    name,
    transport: config.transport || 'stdio',
    command: config.command || '',
    argsText: JSON.stringify(config.args || [], null, 2),
    envText: JSON.stringify(config.env || {}, null, 2),
    cwd: config.cwd || '',
    url: config.url || '',
    headersText: JSON.stringify(config.headers || {}, null, 2),
    bearer_token: config.bearer_token || '',
    bearer_token_env: config.bearer_token_env || '',
    enabled: config.enabled !== false,
    required: config.required === true,
    startup_timeout_secs: config.startup_timeout_secs || '',
    tool_timeout_secs: config.tool_timeout_secs || '',
    enabledToolsText: JSON.stringify(config.enabled_tools || null, null, 2),
    disabledToolsText: JSON.stringify(config.disabled_tools || [], null, 2)
  }
}

function addServer() {
  servers.value.push(toForm(`server_${servers.value.length + 1}`, { transport: 'stdio' }))
}

function randomId() {
  return globalThis.crypto?.randomUUID?.() || `${Date.now()}-${Math.random()}`
}

function removeServer(id) {
  servers.value = servers.value.filter((server) => server.id !== id)
}

function save() {
  error.value = ''
  const next = {}
  try {
    for (const server of servers.value) {
      const name = server.name.trim()
      if (!name) throw new Error('Every MCP server needs a name.')
      if (next[name]) throw new Error(`Duplicate MCP server name: ${name}`)
      const config = {
        transport: server.transport,
        env: parseObject(server.envText, 'Env JSON'),
        headers: parseObject(server.headersText, 'Headers JSON'),
        enabled: server.enabled !== false,
        required: server.required === true,
        disabled_tools: parseArray(server.disabledToolsText, 'Disabled Tools JSON')
      }
      if (server.transport === 'stdio') {
        config.command = required(server.command, `${name} command`)
        config.args = parseArray(server.argsText, 'Args JSON')
        if (server.cwd) config.cwd = server.cwd
      } else {
        config.url = required(server.url, `${name} URL`)
        if (server.bearer_token) config.bearer_token = server.bearer_token
        if (server.bearer_token_env) config.bearer_token_env = server.bearer_token_env
      }
      if (server.startup_timeout_secs) config.startup_timeout_secs = Number(server.startup_timeout_secs)
      if (server.tool_timeout_secs) config.tool_timeout_secs = Number(server.tool_timeout_secs)
      const enabledTools = parseNullableArray(server.enabledToolsText, 'Enabled Tools JSON')
      if (enabledTools) config.enabled_tools = enabledTools
      next[name] = config
    }
  } catch (err) {
    error.value = err.message
    return
  }
  emit('save', next)
}

function required(value, label) {
  const text = String(value || '').trim()
  if (!text) throw new Error(`${label} is required.`)
  return text
}

function parseObject(text, label) {
  const value = parseJson(text || '{}', label)
  if (!value || Array.isArray(value) || typeof value !== 'object') throw new Error(`${label} must be an object.`)
  return value
}

function parseArray(text, label) {
  const value = parseJson(text || '[]', label)
  if (!Array.isArray(value)) throw new Error(`${label} must be an array.`)
  return value.map((item) => String(item))
}

function parseNullableArray(text, label) {
  const value = parseJson(text || 'null', label)
  if (value === null) return null
  if (!Array.isArray(value)) throw new Error(`${label} must be an array or null.`)
  return value.map((item) => String(item))
}

function parseJson(text, label) {
  try {
    return JSON.parse(text)
  } catch {
    throw new Error(`${label} is not valid JSON.`)
  }
}

function close() {
  emit('close')
}

function onBackdropDown() {
  backdropDown = true
}

function onBackdropUp(_event, callback) {
  if (backdropDown) {
    backdropDown = false
    callback()
  }
}
</script>
