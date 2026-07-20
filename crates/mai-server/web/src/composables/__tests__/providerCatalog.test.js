import assert from 'node:assert/strict'
import test from 'node:test'

import {
  catalogModelsForPreset,
  credentialForPreset,
  presetForProvider,
  uiModelFromDescriptor
} from '../../utils/providerCatalog.js'
import { useProviders } from '../useProviders.js'

const futurePreset = {
  id: 'future-provider',
  display_name: 'Future Provider',
  transport: {
    protocol: 'responses',
    connection_modes: [
      { id: 'web_socket', display_name: 'WebSocket' },
      { id: 'http', display_name: 'HTTP' }
    ],
    default_connection_mode: 'web_socket'
  },
  base_url: 'https://future.example/v1',
  credential: { label: 'Access Key', env_var: 'FUTURE_KEY' },
  service_capabilities: {
    web_search: {
      hosted_responses: true,
      standalone: 'future_search_dialect'
    }
  },
  model_catalog_id: 'future-catalog',
  suggested_model: 'future-model',
}
const snapshot = {
  presets: [futurePreset],
  model_catalogs: {
    'future-catalog': {
      models: [{
        id: 'future-model',
        display_name: 'Future Model',
        context_window: 500000,
        max_output_tokens: 64000,
        capabilities: { function_calling: true, parallel_tool_calls: false, reasoning: true },
        reasoning: { default: 'balanced', candidates: ['eco', 'balanced', 'max'] }
      }]
    }
  }
}

test('future presets and models are consumed without id-specific branches', () => {
  assert.deepEqual(credentialForPreset(futurePreset), { label: 'Access Key', envVar: 'FUTURE_KEY' })
  assert.equal(catalogModelsForPreset(snapshot, futurePreset)[0].id, 'future-model')
  assert.equal(presetForProvider(snapshot, {
    preset_id: null,
    base_url: 'https://future.example/v1/',
    catalog: { source: 'bundled', catalog_id: 'future-catalog' }
  })?.id, 'future-provider')
  const model = uiModelFromDescriptor(snapshot.model_catalogs['future-catalog'].models[0])
  assert.deepEqual(model.reasoning.variants.map((variant) => variant.id), ['eco', 'balanced', 'max'])
})

test('provider draft uses the catalog connection default without provider id branches', () => {
  const { providersState, providerDialog, fillFromPreset } = useProviders()
  providersState.catalog = snapshot

  fillFromPreset('future-provider')

  assert.equal(providerDialog.form.connection_mode, 'web_socket')
  assert.deepEqual(providerDialog.form.connection_modes, futurePreset.transport.connection_modes)
  assert.equal(providerDialog.form.protocol, 'responses')
  assert.equal(providerDialog.form.capability_source, 'preset_defaults')
  assert.equal(providerDialog.form.hosted_web_search, true)
  assert.equal(providerDialog.form.standalone_web_search, 'future_search_dialect')
})

test('multiple instances of one preset receive distinct provider ids', () => {
  const { providersState, providerDialog, fillFromPreset } = useProviders()
  providersState.catalog = snapshot
  providersState.providers = [{ id: 'future-provider' }]
  providerDialog.index = null

  fillFromPreset('future-provider')

  assert.equal(providerDialog.form.id, 'future-provider-2')
  assert.equal(providerDialog.form.preset_id, 'future-provider')
})

test('updating an api key preserves transport and catalog fields', async () => {
  const requests = []
  globalThis.fetch = async (path, init = {}) => {
    requests.push({ path, init })
    const body = JSON.parse(init.body)
    return {
      ok: true,
      status: 200,
      text: async () => JSON.stringify({
        providers: body.providers.map((provider) => ({
          ...provider,
          transport: {
            ...provider.transport,
            connection_modes: futurePreset.transport.connection_modes
          },
          models: snapshot.model_catalogs['future-catalog'].models,
          has_api_key: true
        })),
        default_provider_id: body.default_provider_id
      })
    }
  }
  const { providersState, providerDialog, openProviderDialog, saveProviderDialog } = useProviders()
  providersState.catalog = snapshot
  providersState.default_provider_id = 'future-provider'
  providersState.providers = [{
    id: 'future-provider',
    preset_id: 'future-provider',
    transport: {
      protocol: 'responses',
      connection_mode: 'http',
      connection_modes: futurePreset.transport.connection_modes
    },
    name: 'Future Provider',
    base_url: futurePreset.base_url,
    api_key_env: 'FUTURE_KEY',
    capability_selection: { source: 'preset_defaults' },
    service_capabilities: futurePreset.service_capabilities,
    catalog: {
      source: 'bundled',
      catalog_id: 'future-catalog',
      additional_models: []
    },
    models: [uiModelFromDescriptor(snapshot.model_catalogs['future-catalog'].models[0])],
    default_model: 'future-model',
    enabled: true,
    has_api_key: true
  }]
  openProviderDialog(0)
  providerDialog.form.api_key = 'updated-secret'

  await saveProviderDialog()

  const payload = JSON.parse(requests[0].init.body)
  assert.deepEqual(payload.providers[0].transport, {
    protocol: 'responses',
    connection_mode: 'http'
  })
  assert.deepEqual(payload.providers[0].catalog, {
    source: 'bundled',
    catalog_id: 'future-catalog',
    additional_models: []
  })
  assert.equal(payload.providers[0].api_key, 'updated-secret')
  assert.deepEqual(payload.providers[0].capabilities, { source: 'preset_defaults' })
})
