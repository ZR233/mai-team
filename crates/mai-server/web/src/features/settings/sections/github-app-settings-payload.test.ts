import { describe, expect, it } from "vitest"

import {
  buildGithubAppSettingsUpdate,
  buildRelaySettingsUpdate,
} from "@/features/settings/sections/github-app-settings-payload"

describe("GitHub App settings payloads", () => {
  it("omits an empty relay token while preserving other settings", () => {
    expect(buildRelaySettingsUpdate({
      enabled: true,
      url: "https://relay.example",
      token: "   ",
      nodeId: "node-a",
    })).toEqual({
      enabled: true,
      url: "https://relay.example",
      node_id: "node-a",
    })
  })

  it("includes a non-empty relay token", () => {
    expect(buildRelaySettingsUpdate({
      enabled: true,
      url: "https://relay.example",
      token: " relay-token ",
      nodeId: "node-a",
    })).toEqual({
      enabled: true,
      url: "https://relay.example",
      token: " relay-token ",
      node_id: "node-a",
    })
  })

  it("omits an empty private key while preserving other settings", () => {
    expect(buildGithubAppSettingsUpdate({
      publicUrl: "https://relay.example",
      baseUrl: "https://api.github.com",
      appId: "123",
      appSlug: "mai",
      privateKey: "\n  ",
    })).toEqual({
      public_url: "https://relay.example",
      base_url: "https://api.github.com",
      app_id: "123",
      app_slug: "mai",
    })
  })

  it("includes a non-empty private key", () => {
    const privateKey = "-----BEGIN PRIVATE KEY-----\npem\n-----END PRIVATE KEY-----\n"
    expect(buildGithubAppSettingsUpdate({
      publicUrl: "https://relay.example",
      baseUrl: "https://api.github.com",
      appId: "123",
      appSlug: "mai",
      privateKey,
    })).toEqual({
      public_url: "https://relay.example",
      base_url: "https://api.github.com",
      app_id: "123",
      app_slug: "mai",
      private_key: privateKey,
    })
  })
})
