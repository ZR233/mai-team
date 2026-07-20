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
        <article v-for="server in builtins" :key="server.id" class="mcp-server-row">
          <div class="mcp-server-head">
            <div>
              <strong>{{ server.id }}</strong>
              <small>{{ server.transport }} · {{ server.endpoint }}</small>
            </div>
            <label>
              <span>Built-in</span>
              <select v-model="server.enabled">
                <option :value="true">Enabled</option>
                <option :value="false">Disabled</option>
              </select>
            </label>
            <span class="status-chip">{{ server.availability }}</span>
            <small>{{ server.ready_agents }}/{{ server.total_agents }} agents · {{ server.tool_count }} tools</small>
          </div>
        </article>
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
            <span class="status-chip">{{ server.availability }}</span>
          </div>

          <div class="form-grid">
            <label>
              <span>Scope</span>
              <select v-model="server.scope">
                <option value="agent">Agent</option>
                <option value="project">Project</option>
                <option value="system">System</option>
              </select>
            </label>
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
            <label v-if="server.transport === 'streamable_http' && server.has_bearer_token">
              <span>Stored Token</span>
              <select v-model="server.clear_bearer_token">
                <option :value="false">Keep</option>
                <option :value="true">Clear</option>
              </select>
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
        <button class="ghost-button" type="button" :disabled="rechecking" @click="$emit('recheck')">
          {{ rechecking ? 'Checking…' : 'Recheck' }}
        </button>
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
  saving: { type: Boolean, default: false },
  rechecking: { type: Boolean, default: false }
})

const emit = defineEmits(['close', 'save', 'recheck'])

const servers = ref([])
const builtins = ref([])
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
  const configured = props.serversState.servers || []
  builtins.value = configured
    .filter((server) => server.descriptor?.built_in)
    .map((server) => ({
      id: server.descriptor.id,
      transport: server.descriptor.transport,
      endpoint: server.descriptor.endpoint,
      availability: server.availability,
      ready_agents: server.ready_agents || 0,
      total_agents: server.total_agents || 0,
      tool_count: server.tool_count || 0,
      enabled: server.enabled !== false
    }))
  servers.value = configured
    .filter((server) => !server.descriptor?.built_in)
    .map((server) => toForm(server.descriptor?.id || '', server.config || {}, server.descriptor, server))
  error.value = ''
}

function toForm(name, config = {}, descriptor = {}, aggregate = {}) {
  return {
    id: randomId(),
    name,
    scope: config.scope || 'agent',
    transport: descriptor.transport === 'streamableHttp' ? 'streamable_http' : 'stdio',
    availability: aggregate.availability || 'configured',
    command: config.command || '',
    argsText: JSON.stringify(config.args || [], null, 2),
    originalEnvKeys: [...(config.env_keys || [])],
    envText: JSON.stringify(Object.fromEntries((config.env_keys || []).map((key) => [key, ''])), null, 2),
    cwd: config.cwd || '',
    url: config.url || '',
    originalHeaderNames: [...(config.header_names || [])],
    headersText: JSON.stringify(Object.fromEntries((config.header_names || []).map((key) => [key, ''])), null, 2),
    bearer_token: '',
    has_bearer_token: config.has_bearer_token === true,
    clear_bearer_token: false,
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
  const clearSecrets = {}
  try {
    for (const server of servers.value) {
      const name = server.name.trim()
      if (!name) throw new Error('Every MCP server needs a name.')
      if (next[name]) throw new Error(`Duplicate MCP server name: ${name}`)
      const env = parseObject(server.envText, 'Env JSON')
      const headers = parseObject(server.headersText, 'Headers JSON')
      const config = {
        scope: server.scope || 'agent',
        transport: server.transport,
        env,
        headers,
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
        if (server.clear_bearer_token && server.bearer_token) {
          throw new Error(`${name} cannot replace and clear its bearer token at the same time.`)
        }
        if (server.bearer_token) config.bearer_token = server.bearer_token
        if (server.bearer_token_env) config.bearer_token_env = server.bearer_token_env
      }
      if (server.startup_timeout_secs) config.startup_timeout_secs = Number(server.startup_timeout_secs)
      if (server.tool_timeout_secs) config.tool_timeout_secs = Number(server.tool_timeout_secs)
      const enabledTools = parseNullableArray(server.enabledToolsText, 'Enabled Tools JSON')
      if (enabledTools) config.enabled_tools = enabledTools
      next[name] = config
      const clearedEnv = server.originalEnvKeys.filter((key) => !(key in env))
      const clearedHeaders = server.originalHeaderNames.filter((key) => !(key in headers))
      if (server.clear_bearer_token || clearedEnv.length || clearedHeaders.length) {
        clearSecrets[name] = {
          bearer_token: server.clear_bearer_token,
          env: clearedEnv,
          headers: clearedHeaders
        }
      }
    }
  } catch (err) {
    error.value = err.message
    return
  }
  emit('save', {
    userServers: next,
    builtinServers: Object.fromEntries(builtins.value.map((server) => [server.id, server.enabled])),
    clearSecrets
  })
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
