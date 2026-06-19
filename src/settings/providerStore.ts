import * as vscode from "vscode";

export interface ProviderDef {
  id: string;
  label: string;
  needsApiKey: boolean;
  needsUrl: boolean;
  needsEndpointId: boolean;
  defaultUrl?: string;
  defaultModel: string;
  keyEnvHint?: string;
}

/** GetAIBD is the single permitted provider; its key comes from the getaibd.apiKey setting. */
export const BUILTIN_PROVIDERS: ProviderDef[] = [
  { id: "getaibd", label: "GetAIBD", needsApiKey: false, needsUrl: false, needsEndpointId: false, defaultModel: "" },
];

export interface CuratedModel {
  id: string;
  name: string;
  ctx?: string;
  tags?: string[];
}

/** Live models are fetched from the engine, so no static curation is needed. */
export const CURATED_MODELS: Record<string, CuratedModel[]> = {
  getaibd: [],
};

/** Provider display config (icon + accent color). */
export const PROVIDER_META: Record<string, { icon: string; color: string }> = {
  getaibd: { icon: "🟢", color: "#16a34a" },
};

export interface ProviderConfig {
  enabled: boolean;
  defaultModel: string;
  url?: string;
  endpointId?: string;
}

export interface CustomProvider {
  id: string;
  displayName: string;
  baseUrl: string;
  defaultModel: string;
  supportsToolCalling: boolean;
}

const PREFS_KEY = "getaibd.providerPrefs";
const CUSTOM_KEY = "getaibd.customProviders";
const SECRET_PREFIX = "getaibd.key.";

export class ProviderStore {
  constructor(
    private readonly globalState: vscode.Memento,
    private readonly secrets: vscode.SecretStorage,
  ) {}

  getPrefs(): Record<string, ProviderConfig> {
    return this.globalState.get<Record<string, ProviderConfig>>(PREFS_KEY, {});
  }

  getProviderConfig(id: string): ProviderConfig {
    const all = this.getPrefs();
    const def = BUILTIN_PROVIDERS.find((p) => p.id === id);
    return (
      all[id] ?? {
        enabled: id === "getaibd",
        defaultModel: def?.defaultModel ?? "",
        url: def?.defaultUrl,
      }
    );
  }

  async saveProviderConfig(id: string, config: ProviderConfig): Promise<void> {
    const all = this.getPrefs();
    all[id] = config;
    await this.globalState.update(PREFS_KEY, all);
  }

  async setApiKey(providerId: string, key: string): Promise<void> {
    if (key) {
      await this.secrets.store(SECRET_PREFIX + providerId, key);
    } else {
      await this.secrets.delete(SECRET_PREFIX + providerId);
    }
  }

  async getApiKey(providerId: string): Promise<string | undefined> {
    return this.secrets.get(SECRET_PREFIX + providerId);
  }

  async hasApiKey(providerId: string): Promise<boolean> {
    const key = await this.getApiKey(providerId);
    return !!key && key.length > 0;
  }

  /** Custom providers are disabled under the GetAIBD-only lockdown. */
  getCustomProviders(): CustomProvider[] {
    return [];
  }

  async saveCustomProviders(_providers: CustomProvider[]): Promise<void> {}

  async addCustomProvider(_provider: CustomProvider): Promise<void> {}

  async removeCustomProvider(_id: string): Promise<void> {}

  async getEnabledProviderIds(): Promise<string[]> {
    return ["getaibd"];
  }

  async getFullSettingsSnapshot(): Promise<{
    providers: Array<{
      id: string;
      label: string;
      enabled: boolean;
      hasKey: boolean;
      defaultModel: string;
      url?: string;
      endpointId?: string;
      needsApiKey: boolean;
      needsUrl: boolean;
      needsEndpointId: boolean;
      keyEnvHint?: string;
    }>;
    customProviders: Array<CustomProvider & { hasKey: boolean }>;
    serverUrl: string;
    fileContextEnabled: boolean;
    inlineCompletionsEnabled: boolean;
    autoOpenEdits: boolean;
    inlineProvider: string;
    inlineModel: string;
    curatedModels: Record<string, CuratedModel[]>;
  }> {
    const prefs = this.getPrefs();
    const config = vscode.workspace.getConfiguration("getaibd");

    const providers = await Promise.all(
      BUILTIN_PROVIDERS.map(async (def) => {
        const cfg = prefs[def.id] ?? {
          enabled: def.id === "getaibd",
          defaultModel: def.defaultModel,
          url: def.defaultUrl,
        };
        return {
          id: def.id,
          label: def.label,
          enabled: cfg.enabled,
          hasKey: await this.hasApiKey(def.id),
          defaultModel: cfg.defaultModel || def.defaultModel,
          url: cfg.url || def.defaultUrl,
          endpointId: cfg.endpointId,
          needsApiKey: def.needsApiKey,
          needsUrl: def.needsUrl,
          needsEndpointId: def.needsEndpointId,
          keyEnvHint: def.keyEnvHint,
        };
      }),
    );

    const customs = await Promise.all(
      this.getCustomProviders().map(async (cp) => ({
        ...cp,
        hasKey: await this.hasApiKey(cp.id),
      })),
    );

    return {
      providers,
      customProviders: customs,
      serverUrl: config.get<string>("serverUrl", "http://127.0.0.1:39377"),
      fileContextEnabled: config.get<boolean>("fileContext.enabled", true),
      inlineCompletionsEnabled: config.get<boolean>("inlineCompletions.enabled", false),
      autoOpenEdits: config.get<boolean>("editReview.autoOpen", true),
      inlineProvider: config.get<string>("inlineCompletions.provider", "getaibd"),
      inlineModel: config.get<string>("inlineCompletions.model", ""),
      curatedModels: CURATED_MODELS,
    };
  }
}
