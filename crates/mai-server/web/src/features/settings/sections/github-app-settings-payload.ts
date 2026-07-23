export interface RelaySettingsForm {
  enabled: boolean
  url: string
  token: string
  nodeId: string
}

export interface RelaySettingsUpdate {
  enabled: boolean
  url: string | null
  token?: string
  node_id: string | null
}

export interface GithubAppSettingsForm {
  publicUrl: string
  baseUrl: string
  appId: string
  privateKey: string
}

export interface GithubAppSettingsUpdate {
  public_url: string | null
  base_url: string | null
  app_id: string | null
  private_key?: string
}

export function buildRelaySettingsUpdate(form: RelaySettingsForm): RelaySettingsUpdate {
  return {
    enabled: form.enabled,
    url: form.url || null,
    node_id: form.nodeId || null,
    ...(form.token.trim() ? { token: form.token } : {}),
  }
}

export function buildGithubAppSettingsUpdate(form: GithubAppSettingsForm): GithubAppSettingsUpdate {
  return {
    public_url: form.publicUrl || null,
    base_url: form.baseUrl || null,
    app_id: form.appId || null,
    ...(form.privateKey.trim() ? { private_key: form.privateKey } : {}),
  }
}
