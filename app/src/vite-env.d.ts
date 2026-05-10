/// <reference types="vite/client" />

interface ImportMetaEnv {
  /// Anthropic API key for the in-app chat agent. Read at chat-window
  /// build time and embedded into the bundle. MVP-only — production
  /// would route LLM calls through a backend so the key never ships
  /// to a webview. Set via:
  ///   echo 'VITE_ANTHROPIC_API_KEY=sk-ant-…' > app/.env.local
  /// then `pnpm tauri dev`.
  readonly VITE_ANTHROPIC_API_KEY: string | undefined;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
