<script lang="ts">
  import { listen } from "@tauri-apps/api/event";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { onMount } from "svelte";
  import {
    bootstrap,
    connectNode,
    createModuleShortcut,
    deletePackage,
    diagnoseNode,
    downloadPackage,
    disconnectNode,
    listLibrary,
    hideToTray,
    isMobile,
    navigateBack,
    openNode,
    openNodeModule,
    openOffline,
    openOfflineModule,
    resetConnection,
    showHome,
    shortcutOnlineAvailable,
    takeMobileShortcut,
    unlockShortcut,
    unlockVault,
    type ConnectionState,
    type ModuleShortcutTarget,
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
  let rememberWithoutPassword = false;
  let loading = true;
  let mobile = false;
  let submitting = false;
  let connectionProgress = "";
  let error = "";
  let diagnostics: string[] = [];
  let diagnosticsOpen = false;
  let libraryManagerOpen = false;
  let deletingPackage = "";
  let downloadStatus: { status: string; progress: number; message: string } | null = null;
  let shortcutTarget: ModuleShortcutTarget | null = null;
  let shortcutMode: "unlock" | "offline" = "offline";
  let shortcutPassword = "";
  let shortcutError = "";
  let shortcutBusy = false;
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
      const [state, mobilePlatform] = await Promise.all([bootstrap(), isMobile()]);
      connection = state.connection;
      modules = state.modules;
      vaultExists = state.vault_exists;
      nodeUrl = connection.node_url ?? "";
      mobile = mobilePlatform;
      if (mobilePlatform) {
        const shortcut = await takeMobileShortcut();
        if (shortcut) await handleSmartShortcut(shortcut);
      }
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
    const unlistenConnection = listen<string>("connection-progress", (event) => {
      connectionProgress = event.payload;
    });
    const handleDownloadRequest = (event: Event) => {
      const manifestUrl = (event as CustomEvent<unknown>).detail;
      if (typeof manifestUrl !== "string") return;
      void downloadPackage(manifestUrl)
        .then((queued) => {
          if (!queued) addDiagnostic("Package уже загружается");
        })
        .catch((cause) => {
          error = describeError(cause);
        });
    };
    window.addEventListener("netsanctum-download-request", handleDownloadRequest);
    const handleShortcut = (event: Event) => {
      const target = (event as CustomEvent<Partial<ModuleShortcutTarget>>).detail;
      if (
        typeof target?.nodeOrigin === "string" &&
        typeof target?.moduleId === "string" &&
        typeof target?.moduleTitle === "string" &&
        typeof target?.modulePath === "string"
      ) {
        void handleSmartShortcut(target as ModuleShortcutTarget);
      }
    };
    window.addEventListener("netsanctum-open-shortcut", handleShortcut);
    void initialize();
    return () => {
      void unlisten.then((stop) => stop());
      void unlistenLibrary.then((stop) => stop());
      void unlistenDownload.then((stop) => stop());
      void unlistenConnection.then((stop) => stop());
      window.removeEventListener("netsanctum-download-request", handleDownloadRequest);
      window.removeEventListener("netsanctum-open-shortcut", handleShortcut);
    };
  });

  async function connect() {
    error = "";
    addDiagnostic("UI: запущено подключение");
    if (!rememberWithoutPassword && vaultPassword !== vaultPasswordConfirmation) {
      error = "Пароли локального хранилища не совпадают.";
      return;
    }
    connectionProgress = "Проверяем доступ к узлу…";
    submitting = true;
    try {
      const request = {
        node_url: nodeUrl,
        master_token: masterToken,
        vault_password: vaultPassword,
        allow_insecure_http: allowInsecureHttp,
        remember_without_password: mobile && rememberWithoutPassword,
      };
      connection = await connectNode(request);
      vaultExists = true;
      masterToken = "";
      vaultPassword = "";
      vaultPasswordConfirmation = "";
    } catch (cause) {
      error = describeError(cause);
      addDiagnostic(`Ошибка подключения: ${error}`);
    } finally {
      masterToken = "";
      vaultPassword = "";
      vaultPasswordConfirmation = "";
      submitting = false;
      connectionProgress = "";
    }
  }

  async function runDiagnostics() {
    diagnosticsOpen = true;
    diagnostics = ["Диагностика native HTTP запущена…"];
    try {
      diagnostics = await diagnoseNode(nodeUrl, allowInsecureHttp);
    } catch (cause) {
      diagnostics = [`Диагностика не выполнена: ${describeError(cause)}`];
    }
  }

  function addDiagnostic(message: string) {
    diagnostics = [...diagnostics.slice(-19), `${new Date().toLocaleTimeString()}: ${message}`];
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

  async function resetVault() {
    if (!confirm("Удалить сохранённый узел и локальный vault? Скачанные пакеты останутся на устройстве.")) {
      return;
    }
    error = "";
    submitting = true;
    try {
      connection = await resetConnection();
      vaultExists = false;
      nodeUrl = "";
      masterToken = "";
      vaultPassword = "";
      vaultPasswordConfirmation = "";
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

  async function removePackage(item: SavedModule["packages"][number]) {
    if (!confirm(`Удалить «${item.package_title}» и его файлы с устройства?`)) return;
    const key = `${item.node_origin}\n${item.package_id}`;
    deletingPackage = key;
    error = "";
    try {
      modules = await deletePackage(item.node_origin, item.package_id);
    } catch (cause) {
      error = describeError(cause);
    } finally {
      deletingPackage = "";
    }
  }

  async function addModuleShortcut(module: SavedModule) {
    const host = new URL(module.node_origin).host;
    const name = prompt("Название ярлыка", `${module.module_title} · ${host}`)?.trim();
    if (!name) return;
    try {
      await createModuleShortcut(
        module.node_origin,
        module.module_id,
        module.module_title,
        module.module_root_url,
        name,
        defaultIconText(module.module_title),
      );
    } catch (cause) {
      error = describeError(cause);
    }
  }

  function defaultIconText(title: string): string {
    return title
      .trim()
      .split(/\s+/)
      .map((word) => word[0])
      .join("")
      .slice(0, 2)
      .toUpperCase() || "NC";
  }

  function normalizedNode(value: string | null): string {
    if (!value) return "";
    try {
      const url = new URL(value);
      const path = url.pathname.replace(/\/+$/, "");
      return `${url.origin}${path}`;
    } catch {
      return "";
    }
  }

  function shortcutMatchesConnection(target: ModuleShortcutTarget): boolean {
    return normalizedNode(target.nodeOrigin) === normalizedNode(connection.node_url);
  }

  async function handleSmartShortcut(target: ModuleShortcutTarget) {
    shortcutTarget = target;
    shortcutPassword = "";
    shortcutError = "";
    if (!target.modulePath) {
      shortcutMode = "offline";
      return;
    }
    if (shortcutMatchesConnection(target) && connection.status === "connected") {
      try {
        if (await shortcutOnlineAvailable(target.nodeOrigin)) {
          await openShortcutOnline(target);
          closeShortcut();
          return;
        }
        shortcutError = "Сохранённая online-сессия больше не действует.";
      } catch (cause) {
        shortcutError = `Онлайн-версия недоступна: ${describeError(cause)}`;
      }
    }
    shortcutMode = shortcutMatchesConnection(target) && connection.status === "vault_locked"
      ? "unlock"
      : "offline";
  }

  async function openShortcutOnline(target: ModuleShortcutTarget) {
    if (target.modulePath) {
      await openNodeModule(target.modulePath);
    } else {
      await openNode();
    }
  }

  async function unlockShortcutTarget() {
    if (!shortcutTarget) return;
    shortcutBusy = true;
    shortcutError = "";
    try {
      connection = await unlockShortcut(shortcutPassword);
      shortcutPassword = "";
      if (connection.status === "connected" && shortcutMatchesConnection(shortcutTarget)) {
        await openShortcutOnline(shortcutTarget);
        closeShortcut();
      } else {
        shortcutMode = "offline";
        shortcutError = connection.message || "Исходный узел сейчас недоступен.";
      }
    } catch (cause) {
      shortcutError = describeError(cause);
    } finally {
      shortcutPassword = "";
      shortcutBusy = false;
    }
  }

  function offlineShortcutPackages(target: ModuleShortcutTarget) {
    return modules
      .filter((module) =>
        (target.moduleId && module.module_id === target.moduleId) ||
        (target.modulePath && module.module_root_url === target.modulePath),
      )
      .flatMap((module) =>
        module.packages
          .filter((item) => item.status === "ready")
          .map((item) => ({ module, item })),
      )
      .sort((left, right) => {
        const leftSource = normalizedNode(left.module.node_origin) === normalizedNode(target.nodeOrigin) ? 1 : 0;
        const rightSource = normalizedNode(right.module.node_origin) === normalizedNode(target.nodeOrigin) ? 1 : 0;
        return rightSource - leftSource || Number(right.item.saved_at) - Number(left.item.saved_at);
      });
  }

  async function openShortcutOffline(nodeOrigin: string, packageId: string) {
    shortcutBusy = true;
    shortcutError = "";
    try {
      await openOffline(nodeOrigin, packageId);
      closeShortcut();
    } catch (cause) {
      shortcutError = describeError(cause);
    } finally {
      shortcutBusy = false;
    }
  }

  function formatSavedAt(value: string): string {
    const timestamp = Number(value);
    if (!Number.isFinite(timestamp)) return value;
    return new Date(timestamp * 1000).toLocaleString();
  }

  function closeShortcut() {
    shortcutTarget = null;
    shortcutPassword = "";
    shortcutError = "";
    shortcutBusy = false;
  }

  function describeError(cause: unknown): string {
    return cause instanceof Error ? cause.message : typeof cause === "string" ? cause : "Не удалось выполнить операцию.";
  }

  function formatSize(bytes: number): string {
    if (bytes < 1024) return `${bytes} Б`;
    if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(1)} КБ`;
    if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(1)} МБ`;
    return `${(bytes / 1024 ** 3).toFixed(1)} ГБ`;
  }
</script>

<svelte:head>
  <title>Netsanctum Client</title>
</svelte:head>

{#if !mobile}
<div class="window-titlebar" data-tauri-drag-region role="toolbar" aria-label="Управление окном">
  <div class="titlebar-nav">
    <button type="button" title="Назад" aria-label="Назад" on:click={() => navigateBack()}>←</button>
    <button type="button" title="Client Home" aria-label="Client Home" on:click={() => showHome()}>NC</button>
    <span class="titlebar-product">NETSANCTUM CLIENT</span>
  </div>
  <div class="titlebar-modules" data-tauri-drag-region></div>
  <div class="window-controls">
    <button type="button" title="Свернуть" aria-label="Свернуть" on:click={() => appWindow.minimize()}>−</button>
    <button type="button" title="Развернуть" aria-label="Развернуть" on:click={() => appWindow.toggleMaximize()}>□</button>
    <button class="window-hide" type="button" title="Скрыть в tray" aria-label="Скрыть в tray" on:click={() => hideToTray()}>×</button>
  </div>
</div>
{/if}

<main class:mobile class="app-shell">
  <header class="topbar">
    <div class="brand-mark" aria-hidden="true">NC</div>
    <div>
      <p class="eyebrow">PRIVATE OUTPOST</p>
      <h1>Netsanctum <span>Client</span></h1>
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
          <button class="secondary-button" type="button" on:click={resetVault} disabled={submitting}>
            ЗАБЫЛ ПАРОЛЬ VAULT
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
                required={!rememberWithoutPassword}
                disabled={submitting || connection.status === "connected" || rememberWithoutPassword}
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
                required={!rememberWithoutPassword}
                disabled={submitting || connection.status === "connected" || rememberWithoutPassword}
                autocomplete="new-password"
              />
            </div>
          </div>

          {#if mobile}
            <label class="insecure-option">
              <input bind:checked={rememberWithoutPassword} type="checkbox" disabled={submitting} />
              <span>
                Входить без пароля vault
                <small>Ключ хранится в защищённом Android Keystore этого устройства.</small>
              </span>
            </label>
          {/if}

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
            {#if mobile}
              <div class="connection-message" data-status={connection.status}>
                Защищённая сессия создана. Узел откроется в отдельном WebView без доступа к Tauri IPC.
              </div>
            {/if}
            <button class="primary-button" type="button" on:click={showNode} disabled={submitting}>
              ОТКРЫТЬ УЗЕЛ
              <span aria-hidden="true">→</span>
            </button>
            <button class="secondary-button" type="button" on:click={disconnect} disabled={submitting}>
              Отключить узел
            </button>
          {:else}
            <button class="primary-button" type="submit" disabled={submitting}>
              {submitting ? connectionProgress || "ПРОВЕРКА…" : "ПОДКЛЮЧИТЬСЯ"}
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
        {#if mobile}
          <button class="diagnostics-button" type="button" on:click={runDiagnostics} disabled={submitting || !nodeUrl}>
            ДИАГНОСТИКА NATIVE HTTP
          </button>
          {#if diagnosticsOpen}
            <pre class="diagnostics-log" aria-live="polite">{diagnostics.join("\n")}</pre>
          {/if}
        {/if}
      </section>

      <section class="library-panel">
        <div class="library-heading">
          <div>
            <p class="eyebrow">LOCAL ARCHIVE</p>
            <h2>Сохранённые модули</h2>
          </div>
          <div class="library-heading-actions">
            <button type="button" on:click={() => (libraryManagerOpen = true)} disabled={modules.length === 0}>
              УПРАВЛЕНИЕ
            </button>
            <div class="module-count"><strong>{modules.length}</strong><span>МОДУЛЕЙ</span></div>
          </div>
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
                      <small data-status={item.status}>
                        {item.status === "failed" ? "ОШИБКА" : item.status === "downloading" ? "ЗАГРУЗКА" : formatSize(item.byte_size)}
                      </small>
                    </div>
                  {/each}
                </div>
                <footer>
                  <span>{module.packages.filter((item) => item.status === "ready").length} доступно / {module.packages.length} всего</span>
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

{#if shortcutTarget}
  {@const shortcutPackages = offlineShortcutPackages(shortcutTarget)}
  <div class="shortcut-overlay" role="dialog" aria-modal="true" aria-labelledby="shortcut-title">
    <section class="shortcut-panel">
      <header>
        <div>
          <p class="eyebrow">SMART SHORTCUT</p>
          <h2 id="shortcut-title">{shortcutTarget.moduleTitle || shortcutTarget.moduleId}</h2>
          <small>{new URL(shortcutTarget.nodeOrigin).host}</small>
        </div>
        <button type="button" on:click={closeShortcut}>ЗАКРЫТЬ</button>
      </header>

      {#if shortcutMode === "unlock"}
        <form on:submit|preventDefault={unlockShortcutTarget} autocomplete="off">
          <p>Для онлайн-входа в исходный узел разблокируйте сохранённый мастер-токен.</p>
          <label for="shortcut-vault-password">Пароль локального хранилища</label>
          <input
            id="shortcut-vault-password"
            bind:value={shortcutPassword}
            type="password"
            required
            disabled={shortcutBusy}
            autocomplete="off"
          />
          {#if shortcutError}
            <div class="error-message" role="alert">{shortcutError}</div>
          {/if}
          <button class="primary-button" type="submit" disabled={shortcutBusy}>
            {shortcutBusy ? "ПРОВЕРКА УЗЛА…" : "ОТКРЫТЬ ОНЛАЙН"}
          </button>
          <button class="secondary-button" type="button" on:click={() => (shortcutMode = "offline")}>
            ВЫБРАТЬ ОФЛАЙН-ВЕРСИЮ
          </button>
        </form>
      {:else}
        <p class="shortcut-explanation">
          Доступной online-сессии для исходного узла нет. Выберите сохранённую версию этого модуля.
        </p>
        {#if shortcutError}
          <div class="error-message" role="alert">{shortcutError}</div>
        {/if}
        {#if shortcutPackages.length === 0}
          <div class="shortcut-empty">
            На устройстве нет готовых офлайн-версий этого модуля.
          </div>
        {:else}
          <div class="shortcut-options">
            {#each shortcutPackages as candidate}
              <button
                type="button"
                disabled={shortcutBusy}
                on:click={() => openShortcutOffline(candidate.module.node_origin, candidate.item.package_id)}
              >
                <span>
                  <strong>{candidate.item.package_title}</strong>
                  <small>{candidate.module.module_title} · {new URL(candidate.module.node_origin).host}</small>
                </span>
                <span>
                  <strong>{formatSize(candidate.item.byte_size)}</strong>
                  <small>{formatSavedAt(candidate.item.saved_at)}</small>
                </span>
              </button>
            {/each}
          </div>
        {/if}
        {#if shortcutMatchesConnection(shortcutTarget) && connection.status === "vault_locked"}
          <button class="secondary-button" type="button" on:click={() => (shortcutMode = "unlock")}>
            ПОПРОБОВАТЬ ОНЛАЙН
          </button>
        {/if}
      {/if}
    </section>
  </div>
{/if}

{#if libraryManagerOpen}
  <div class="library-manager" role="dialog" aria-modal="true" aria-labelledby="library-manager-title">
    <header>
      <div>
        <p class="eyebrow">LOCAL STORAGE</p>
        <h2 id="library-manager-title">Управление библиотекой</h2>
      </div>
      <button type="button" on:click={() => (libraryManagerOpen = false)}>ЗАКРЫТЬ</button>
    </header>

    {#if error}
      <div class="error-message" role="alert">{error}</div>
    {/if}

    <div class="library-manager-list">
      {#each modules as module}
        <article>
          <div class="library-manager-module-heading">
            <div>
              <h3>{module.module_title}</h3>
              <p>{module.module_id} · {new URL(module.node_origin).host}</p>
            </div>
            {#if mobile}
              <button type="button" on:click={() => addModuleShortcut(module)}>ЯРЛЫК</button>
            {/if}
          </div>
          {#each module.packages as item}
            {@const key = `${item.node_origin}\n${item.package_id}`}
            <div class="library-manager-item">
              <div>
                <strong>{item.package_title}</strong>
                <small>{item.status === "failed" ? "Ошибка загрузки" : `${formatSize(item.byte_size)} · ${item.status}`}</small>
              </div>
              <button type="button" disabled={deletingPackage === key} on:click={() => removePackage(item)}>
                {deletingPackage === key ? "УДАЛЕНИЕ…" : "УДАЛИТЬ"}
              </button>
            </div>
          {/each}
        </article>
      {/each}
    </div>
  </div>
{/if}
