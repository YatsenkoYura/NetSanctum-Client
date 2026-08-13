<script lang="ts">
  import { listen } from "@tauri-apps/api/event";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { onMount } from "svelte";
  import {
    bootstrap,
    connectNode,
    disconnectNode,
    listLibrary,
    hideToTray,
    navigateBack,
    openNode,
    openOfflineModule,
    showHome,
    unlockVault,
    type ConnectionState,
    type SavedModule,
  } from "./lib/api";

  let connection: ConnectionState = {
    status: "disconnected",
    node_url: null,
    node_name: null,
    message: null,
  };
  let modules: SavedModule[] = [];
  let nodeUrl = "";
  let masterToken = "";
  let vaultPassword = "";
  let vaultPasswordConfirmation = "";
  let vaultExists = false;
  let allowInsecureHttp = false;
  let loading = true;
  let submitting = false;
  let error = "";
  let downloadStatus: { status: string; progress: number; message: string } | null = null;
  const appWindow = getCurrentWindow();

  const labels: Record<ConnectionState["status"], string> = {
    connected: "УЗЕЛ ДОСТУПЕН",
    disconnected: "НЕ ПОДКЛЮЧЕНО",
    vault_locked: "ХРАНИЛИЩЕ ЗАБЛОКИРОВАНО",
    unreachable: "УЗЕЛ НЕДОСТУПЕН",
    credentials_invalid: "ДОСТУП ОТОЗВАН",
  };

  async function initialize() {
    try {
      const state = await bootstrap();
      connection = state.connection;
      modules = state.modules;
      vaultExists = state.vault_exists;
      nodeUrl = connection.node_url ?? "";
    } catch (cause) {
      error = describeError(cause);
    } finally {
      loading = false;
    }
  }

  onMount(() => {
    const unlisten = listen<ConnectionState>("node-session-expired", (event) => {
      connection = event.payload;
      vaultExists = true;
      nodeUrl = connection.node_url ?? "";
    });
    const unlistenLibrary = listen("library-updated", () => {
      void listLibrary().then((savedModules) => {
        modules = savedModules;
      });
    });
    const unlistenDownload = listen<{ status: string; progress: number; message: string }>(
      "download-status",
      (event) => {
        downloadStatus = event.payload;
      },
    );
    void initialize();
    return () => {
      void unlisten.then((stop) => stop());
      void unlistenLibrary.then((stop) => stop());
      void unlistenDownload.then((stop) => stop());
    };
  });

  async function connect() {
    error = "";
    if (vaultPassword !== vaultPasswordConfirmation) {
      error = "Пароли локального хранилища не совпадают.";
      return;
    }
    submitting = true;
    try {
      connection = await connectNode({
        node_url: nodeUrl,
        master_token: masterToken,
        vault_password: vaultPassword,
        allow_insecure_http: allowInsecureHttp,
      });
      vaultExists = true;
      masterToken = "";
      vaultPassword = "";
      vaultPasswordConfirmation = "";
    } catch (cause) {
      error = describeError(cause);
    } finally {
      masterToken = "";
      vaultPassword = "";
      vaultPasswordConfirmation = "";
      submitting = false;
    }
  }

  async function unlock() {
    error = "";
    submitting = true;
    try {
      connection = await unlockVault(vaultPassword);
      if (connection.status === "credentials_invalid") vaultExists = false;
      vaultPassword = "";
    } catch (cause) {
      error = describeError(cause);
    } finally {
      vaultPassword = "";
      submitting = false;
    }
  }

  async function disconnect() {
    error = "";
    submitting = true;
    try {
      connection = await disconnectNode();
      nodeUrl = "";
      masterToken = "";
      vaultPassword = "";
      vaultPasswordConfirmation = "";
      vaultExists = false;
      allowInsecureHttp = false;
    } catch (cause) {
      error = describeError(cause);
    } finally {
      submitting = false;
    }
  }

  async function showNode() {
    error = "";
    submitting = true;
    try {
      await openNode();
    } catch (cause) {
      error = describeError(cause);
    } finally {
      submitting = false;
    }
  }

  async function showOfflineModule(nodeOrigin: string, moduleId: string) {
    error = "";
    try {
      await openOfflineModule(nodeOrigin, moduleId);
    } catch (cause) {
      error = describeError(cause);
    }
  }

  function describeError(cause: unknown): string {
    return typeof cause === "string" ? cause : "Не удалось выполнить операцию.";
  }

  function formatSize(bytes: number): string {
    if (bytes < 1024) return `${bytes} Б`;
    if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(1)} КБ`;
    if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(1)} МБ`;
    return `${(bytes / 1024 ** 3).toFixed(1)} ГБ`;
  }
</script>

<svelte:head>
  <title>NetSanctum Desktop</title>
</svelte:head>

<div class="window-titlebar" data-tauri-drag-region role="toolbar" aria-label="Управление окном">
  <div class="titlebar-nav">
    <button type="button" title="Назад" aria-label="Назад" on:click={() => navigateBack()}>←</button>
    <button type="button" title="Desktop Home" aria-label="Desktop Home" on:click={() => showHome()}>NS</button>
    <span class="titlebar-product">NETSANCTUM</span>
  </div>
  <div class="titlebar-modules" data-tauri-drag-region></div>
  <div class="window-controls">
    <button type="button" title="Свернуть" aria-label="Свернуть" on:click={() => appWindow.minimize()}>−</button>
    <button type="button" title="Развернуть" aria-label="Развернуть" on:click={() => appWindow.toggleMaximize()}>□</button>
    <button class="window-hide" type="button" title="Скрыть в tray" aria-label="Скрыть в tray" on:click={() => hideToTray()}>×</button>
  </div>
</div>

<main class="app-shell">
  <header class="topbar">
    <div class="brand-mark" aria-hidden="true">NS</div>
    <div>
      <p class="eyebrow">PRIVATE OUTPOST</p>
      <h1>NetSanctum <span>Desktop</span></h1>
    </div>
    <div class="topbar-status" data-status={connection.status}>
      <span class="status-light"></span>
      {labels[connection.status]}
    </div>
  </header>

  {#if loading}
    <section class="loading-state" aria-live="polite">
      <span></span><span></span><span></span>
      Проверяем локальное хранилище и соединение
    </section>
  {:else}
    <div class="workspace">
      <section class="connection-panel">
        <div class="section-number">01</div>
        <p class="eyebrow">REMOTE NODE</p>
        <h2>Подключение</h2>
        <p class="section-copy">
          Мастер-токен используется только Rust-ядром и хранится в локальном зашифрованном vault.
        </p>

        {#if vaultExists && connection.status !== "connected" && connection.status !== "credentials_invalid"}
        <form on:submit|preventDefault={unlock} autocomplete="off">
          <div class="locked-node">
            <span>СОХРАНЁННЫЙ УЗЕЛ</span>
            <strong>{connection.node_url}</strong>
          </div>

          <label for="vault-unlock-password">Пароль локального хранилища</label>
          <input
            id="vault-unlock-password"
            bind:value={vaultPassword}
            type="password"
            placeholder="Введите пароль vault"
            required
            disabled={submitting}
            autocomplete="off"
          />

          {#if error}
            <div class="error-message" role="alert">{error}</div>
          {/if}

          {#if connection.message}
            <div class="connection-message" data-status={connection.status}>{connection.message}</div>
          {/if}

          <button class="primary-button" type="submit" disabled={submitting}>
            {submitting ? "РАЗБЛОКИРОВКА…" : "РАЗБЛОКИРОВАТЬ"}
            <span aria-hidden="true">→</span>
          </button>
        </form>
        {:else}
        <form on:submit|preventDefault={connect} autocomplete="off">
          <label for="node-url">Адрес узла</label>
          <input
            id="node-url"
            bind:value={nodeUrl}
            type="url"
            inputmode="url"
            placeholder="https://sanctum.example.net"
            required
            disabled={submitting || connection.status === "connected"}
            spellcheck="false"
          />

          <label for="master-token">Мастер-токен</label>
          <input
            id="master-token"
            bind:value={masterToken}
            type="password"
            placeholder="Вставьте токен доступа"
            required
            disabled={submitting || connection.status === "connected"}
            autocomplete="off"
          />

          <div class="vault-fields">
            <div>
              <label for="vault-password">Пароль локального хранилища</label>
              <input
                id="vault-password"
                bind:value={vaultPassword}
                type="password"
                minlength="10"
                placeholder="Минимум 10 символов"
                required
                disabled={submitting || connection.status === "connected"}
                autocomplete="new-password"
              />
            </div>
            <div>
              <label for="vault-password-confirmation">Повторите пароль</label>
              <input
                id="vault-password-confirmation"
                bind:value={vaultPasswordConfirmation}
                type="password"
                minlength="10"
                placeholder="Повторите пароль"
                required
                disabled={submitting || connection.status === "connected"}
                autocomplete="new-password"
              />
            </div>
          </div>

          <label class="insecure-option">
            <input
              bind:checked={allowInsecureHttp}
              type="checkbox"
              disabled={submitting || connection.status === "connected"}
            />
            <span>
              Разрешить HTTP
              <small>Только для доверенной локальной сети. Токен передаётся без защиты TLS.</small>
            </span>
          </label>

          {#if error}
            <div class="error-message" role="alert">{error}</div>
          {/if}

          {#if connection.message}
            <div class="connection-message" data-status={connection.status}>{connection.message}</div>
          {/if}

          {#if connection.status === "connected"}
            <button class="primary-button" type="button" on:click={showNode} disabled={submitting}>
              ОТКРЫТЬ УЗЕЛ
              <span aria-hidden="true">→</span>
            </button>
            <button class="secondary-button" type="button" on:click={disconnect} disabled={submitting}>
              Отключить узел
            </button>
          {:else}
            <button class="primary-button" type="submit" disabled={submitting}>
              {submitting ? "ПРОВЕРКА…" : "ПОДКЛЮЧИТЬСЯ"}
              <span aria-hidden="true">→</span>
            </button>
          {/if}
        </form>
        {/if}

        <div class="security-note">
          <span aria-hidden="true">◆</span>
          <div>
            <strong>Локальный парольный vault</strong>
            <p>Argon2id + XChaCha20-Poly1305. Пароль и ключ шифрования никогда не записываются на диск.</p>
          </div>
        </div>
      </section>

      <section class="library-panel">
        <div class="library-heading">
          <div>
            <p class="eyebrow">LOCAL ARCHIVE</p>
            <h2>Сохранённые модули</h2>
          </div>
          <div class="module-count"><strong>{modules.length}</strong><span>МОДУЛЕЙ</span></div>
        </div>

        {#if downloadStatus}
          <div class="download-strip" data-status={downloadStatus.status}>
            <div>
              <strong>{downloadStatus.status === "ready" ? "СОХРАНЕНО" : downloadStatus.status === "failed" ? "ОШИБКА" : "ЗАГРУЗКА"}</strong>
              <span>{downloadStatus.message}</span>
            </div>
            <small>{Math.round(downloadStatus.progress * 100)}%</small>
          </div>
        {/if}

        {#if modules.length === 0}
          <div class="empty-library">
            <div class="empty-glyph" aria-hidden="true">
              <span></span><span></span><span></span>
            </div>
            <h3>Локальный архив пуст</h3>
            <p>Подключитесь к узлу и используйте «Сохранить на устройство» внутри доступного модуля.</p>
          </div>
        {:else}
          <div class="module-grid">
            {#each modules as module}
              <article class="module-card">
                <div class="module-card-header">
                  <div class="module-icon">{module.module_title.slice(0, 2).toUpperCase()}</div>
                  <div>
                    <h3>{module.module_title}</h3>
                    <p>{module.module_id}</p>
                  </div>
                </div>
                <div class="package-list">
                  {#each module.packages.slice(0, 3) as item}
                    <div class="package-row">
                      <span>{item.package_title}</span>
                      <small>{formatSize(item.byte_size)}</small>
                    </div>
                  {/each}
                </div>
                <footer>
                  <span>{module.packages.length} сохранено</span>
                  <button
                    type="button"
                    disabled={!module.packages.some((item) => item.status === "ready")}
                    on:click={() => showOfflineModule(module.node_origin, module.module_id)}
                  >ОТКРЫТЬ МОДУЛЬ</button>
                </footer>
              </article>
            {/each}
          </div>
        {/if}
      </section>
    </div>
  {/if}
</main>
