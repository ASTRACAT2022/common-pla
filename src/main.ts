import { invoke } from "@tauri-apps/api/core";
import "./style.css";

type Profile = {
  id: string;
  name: string;
  protocol: string;
  endpoint: string;
};

type RuntimeStatus = {
  platform: string;
  xrayDetected: boolean;
  awgDetected: boolean;
  note: string;
};

type ConnectionStatus = {
  connected: boolean;
  profileId: string | null;
  name: string | null;
  protocol: string | null;
};

type RoutingSummary = {
  name: string;
  globalProxy: boolean;
  directCount: number;
  proxyCount: number;
  blockCount: number;
};

const icon = (name: string, size = 20) => {
  const icons: Record<string, string> = {
    shield: '<path d="M12 22s8-4 8-11V5l-8-3-8 3v6c0 7 8 11 8 11Z"/><path d="m9 12 2 2 4-4"/>',
    globe: '<circle cx="12" cy="12" r="10"/><path d="M2 12h20"/><path d="M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10Z"/>',
    arrow: '<path d="M7 17 17 7"/><path d="M7 7h10v10"/>',
    download: '<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><path d="m7 10 5 5 5-5"/><path d="M12 15V3"/>',
    refresh: '<path d="M20 7v5h-5"/><path d="M4 17v-5h5"/><path d="M5.6 9a7 7 0 0 1 11.6-2L20 12M4 12l2.8 5a7 7 0 0 0 11.6-2"/>',
    chevron: '<path d="m9 18 6-6-6-6"/>',
    check: '<path d="m5 12 4 4L19 6"/>',
    link: '<path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71"/><path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71"/>',
    lock: '<rect width="18" height="11" x="3" y="11" rx="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/>',
    close: '<path d="m18 6-12 12"/><path d="m6 6 12 12"/>',
    sliders: '<path d="M4 21v-7"/><path d="M4 10V3"/><path d="M12 21v-9"/><path d="M12 8V3"/><path d="M20 21v-5"/><path d="M20 12V3"/><path d="M2 14h4"/><path d="M10 8h4"/><path d="M18 16h4"/>',
  };
  return `<svg width="${size}" height="${size}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${icons[name] ?? icons.globe}</svg>`;
};

const escapeHtml = (value: string) =>
  value.replace(/[&<>"']/g, (character) => {
    const entities: Record<string, string> = {
      "&": "&amp;",
      "<": "&lt;",
      ">": "&gt;",
      '"': "&quot;",
      "'": "&#39;",
    };
    return entities[character];
  });

const PROFILE_CACHE_KEY = "commonnet.profile-list.v1";
const SUBSCRIPTION_REFRESH_INTERVAL = 60 * 60 * 1000;

function isProfile(value: unknown): value is Profile {
  if (typeof value !== "object" || value === null) return false;
  const profile = value as Record<string, unknown>;
  return typeof profile.id === "string"
    && typeof profile.name === "string"
    && typeof profile.protocol === "string"
    && typeof profile.endpoint === "string";
}

function readCachedProfiles(): { profiles: Profile[]; selectedId: string | null; updatedAt: number | null } {
  try {
    const raw = localStorage.getItem(PROFILE_CACHE_KEY);
    if (!raw) return { profiles: [], selectedId: null, updatedAt: null };
    const cached = JSON.parse(raw) as { profiles?: unknown; selectedId?: unknown; updatedAt?: unknown };
    if (!Array.isArray(cached.profiles)) return { profiles: [], selectedId: null, updatedAt: null };
    const profiles = cached.profiles.filter(isProfile);
    const selectedId = typeof cached.selectedId === "string" && profiles.some((profile) => profile.id === cached.selectedId)
      ? cached.selectedId
      : profiles[0]?.id ?? null;
    return {
      profiles,
      selectedId,
      updatedAt: typeof cached.updatedAt === "number" ? cached.updatedAt : null,
    };
  } catch {
    return { profiles: [], selectedId: null, updatedAt: null };
  }
}

const cachedProfiles = readCachedProfiles();
const state: {
  profiles: Profile[];
  selectedId: string | null;
  profilesReady: boolean;
  runtime: RuntimeStatus | null;
  subscriptionUrl: string | null;
  subscriptionDraft: string | null;
  subscriptionSyncing: boolean;
  subscriptionUpdateFailed: boolean;
  lastSubscriptionUpdate: number | null;
  routingSummary: RoutingSummary | null;
  connection: ConnectionStatus;
} = {
  profiles: cachedProfiles.profiles,
  selectedId: cachedProfiles.selectedId,
  profilesReady: false,
  runtime: null,
  subscriptionUrl: null,
  subscriptionDraft: null,
  subscriptionSyncing: false,
  subscriptionUpdateFailed: false,
  lastSubscriptionUpdate: cachedProfiles.updatedAt,
  routingSummary: null,
  connection: { connected: false, profileId: null, name: null, protocol: null },
};

function persistCachedProfiles() {
  try {
    localStorage.setItem(PROFILE_CACHE_KEY, JSON.stringify({
      profiles: state.profiles,
      selectedId: state.selectedId,
      updatedAt: state.lastSubscriptionUpdate,
    }));
  } catch {
    // The live subscription still works if the webview storage is unavailable.
  }
}

function cachedListStatus() {
  if (state.subscriptionSyncing) return "Обновляем список…";
  if (state.subscriptionUpdateFailed && state.profiles.length) return "Сохранённый список · обновление не удалось";
  if (state.lastSubscriptionUpdate) {
    const updatedAt = new Date(state.lastSubscriptionUpdate).toLocaleTimeString("ru-RU", { hour: "2-digit", minute: "2-digit" });
    return `Автообновление каждый час · ${updatedAt}`;
  }
  return state.subscriptionUrl ? "Автообновление каждый час" : "Подписка не настроена";
}

function formatRoutingSummary(summary: RoutingSummary | null) {
  if (!summary) return "Профиль маршрутизации не настроен";
  const defaultRoute = summary.globalProxy ? "Прокси по умолчанию" : "Прямое подключение по умолчанию";
  return `${summary.name} · ${defaultRoute} · ${summary.directCount} Direct / ${summary.proxyCount} Proxy / ${summary.blockCount} Block`;
}

const root = document.querySelector<HTMLDivElement>("#app")!;

function render() {
  const selected = state.profiles.find((profile) => profile.id === state.selectedId) ?? state.profiles[0];
  if (selected && !state.selectedId) state.selectedId = selected.id;
  const tunnelReady = selected?.protocol === "AmneziaWG"
    ? Boolean(state.runtime?.awgDetected)
    : Boolean(state.runtime?.xrayDetected);
  const isConnected = state.connection.connected;
  const connectDisabled = !isConnected && (!selected || !state.profilesReady || !tunnelReady);
  const connectTitle = !state.profilesReady
    ? "Сначала обновите список серверов из подписки"
    : !tunnelReady
      ? "Установите движок, указанный в блоке Система"
      : "";
  const subscriptionInput = state.subscriptionDraft ?? state.subscriptionUrl ?? "";

  root.innerHTML = `
    <div class="window-frame">
      <aside class="rail">
        <div class="brand-mark">${icon("shield", 22)}</div>
        <button class="rail-button is-active" aria-label="Подключение">${icon("globe", 19)}</button>
        <button class="rail-button" id="open-settings" aria-label="Настройки">${icon("sliders", 19)}</button>
        <div class="rail-bottom">${icon("lock", 17)}</div>
      </aside>
      <main class="main-shell">
        <header class="topbar">
          <div class="topbar-title"><span class="eyebrow">COMMONNET</span><span>Личный VPN-клиент</span></div>
          <div class="topbar-right"><span class="privacy-badge">${icon("lock", 14)} Данные остаются на устройстве</span><span class="avatar">C</span></div>
        </header>

        <section class="content-grid">
          <div class="left-column">
            <section class="hero-card">
              <div class="hero-orbit orbit-one"></div><div class="hero-orbit orbit-two"></div>
              <div class="hero-topline"><span class="live-dot"></span> ${selected ? "ПРОФИЛЬ ВЫБРАН" : "ГОТОВ К НАСТРОЙКЕ"}</div>
              <div class="hero-copy">
                <div class="hero-copy-text">
                  <h1>${selected ? "Ваше соединение под контролем" : "Подключитесь безопасно"}</h1>
                  <p>${selected ? `Выбран сервер <strong>${escapeHtml(selected.name)}</strong>. ${isConnected ? "Туннель активен." : "Проверьте движок и подключитесь."}` : "Добавьте подписку, чтобы увидеть доступные серверы."}</p>
                </div>
                <div class="hero-emblem">${icon("shield", 34)}<span></span></div>
              </div>
              <div class="connect-row">
                <button class="connect-button ${isConnected ? "is-connected" : ""}" id="connect-button" ${connectDisabled ? "disabled" : ""} title="${escapeHtml(connectDisabled ? connectTitle : "")}">${icon(isConnected ? "close" : "shield", 18)}<span>${isConnected ? "Отключиться" : "Подключиться"}</span></button>
                <div class="connect-caption"><span class="status-indicator ${isConnected ? "is-on" : ""}"></span><span>${isConnected ? `Защищено · ${escapeHtml(state.connection.protocol ?? "VPN")}` : "Туннель не запущен"}</span></div>
              </div>
              <div class="hero-bottom"><span>${icon("lock", 14)} Шифрование</span><i></i><span>Трафик не передаётся через Commonnet</span></div>
            </section>

            <section class="servers-card">
              <div class="section-heading"><div><span class="eyebrow">ВАШИ ЛОКАЦИИ</span><h2>Серверы</h2></div><span class="count-pill" id="profile-count">${state.profiles.length}</span></div>
              ${state.profiles.length ? `<div class="server-list">${state.profiles.map((profile) => `
                <button class="server-row ${profile.id === selected?.id ? "selected" : ""}" data-profile-id="${escapeHtml(profile.id)}">
                  <span class="server-icon">${icon("globe", 18)}</span>
                  <span class="server-name"><strong>${escapeHtml(profile.name)}</strong><small>${escapeHtml(profile.protocol)} <span>·</span> ${escapeHtml(profile.endpoint)}</small></span>
                  <span class="server-select">${profile.id === selected?.id ? icon("check", 17) : icon("chevron", 17)}</span>
                </button>`).join("")}</div>` : `<div class="empty-state"><div class="empty-icon">${icon("globe", 23)}</div><strong>Список пока пуст</strong><span>Добавьте ссылку на подписку справа — серверы появятся здесь.</span></div>`}
              <div class="servers-foot"><span>${icon("refresh", 14)} ${escapeHtml(cachedListStatus())}</span><button class="text-button" id="refresh-list" ${state.subscriptionUrl && !state.subscriptionSyncing ? "" : "disabled"}>Обновить список</button></div>
            </section>
          </div>

          <div class="right-column">
            <section class="subscription-card">
              <div class="section-heading"><div><span class="eyebrow">НАСТРОЙКА</span><h2>Подписка</h2></div><span class="card-icon">${icon("link", 17)}</span></div>
              <p class="card-description">Вставьте ссылку провайдера один раз. Список обновляется при запуске и затем каждый час.</p>
              <form id="subscription-form" class="subscription-form">
                <label for="subscription-url">Ссылка на подписку</label>
                <div class="input-wrap">${icon("link", 16)}<input id="subscription-url" name="url" type="url" placeholder="https://example.com/subscription" autocomplete="off" value="${escapeHtml(subscriptionInput)}" ${state.subscriptionSyncing ? "disabled" : ""} required /></div>
                <button class="primary-button" id="import-button" type="submit" ${state.subscriptionSyncing ? "disabled" : ""}>${state.subscriptionSyncing ? '<span class="spinner"></span><span>Обновляем…</span>' : `${icon("download", 17)}<span>Загрузить серверы</span>`}</button>
              </form>
              <div class="form-feedback" id="form-feedback" aria-live="polite"></div>
              <div class="secure-note">${icon("lock", 15)} <span>Ссылка хранится локально с ограниченными правами. Секреты профилей не сохраняются и загружаются заново из подписки.</span></div>
            </section>

            <section class="engine-card">
              <div class="engine-title"><div><span class="eyebrow">СИСТЕМА</span><h2>Движки</h2></div><span class="platform-tag">${escapeHtml(state.runtime?.platform ?? "Проверка…")}</span></div>
              <div class="engine-row"><span class="engine-logo xray-logo">X</span><span class="engine-name"><strong>Xray</strong><small>VLESS · VMess · Trojan · SS</small></span><span class="engine-status ${state.runtime?.xrayDetected ? "ready" : "missing"}"><i></i>${state.runtime?.xrayDetected ? "Найден" : "Не найден"}</span></div>
              <div class="engine-row"><span class="engine-logo awg-logo">A</span><span class="engine-name"><strong>AmneziaWG</strong><small>WireGuard с обфускацией</small></span><span class="engine-status ${state.runtime?.awgDetected ? "ready" : "missing"}"><i></i>${state.runtime?.awgDetected ? "Найден" : "Не найден"}</span></div>
              <div class="engine-notice">${icon("shield", 15)} <span>${escapeHtml(state.runtime?.note ?? "Движки проверяются на этом устройстве.")}</span></div>
            </section>

            <div class="build-tag"><span class="build-dot"></span> Commonnet Desktop <span>·</span> версия 0.1.5</div>
          </div>
        </section>
      </main>
      <div class="toast" id="toast" role="status"></div>
      <dialog id="settings-dialog" class="settings-dialog">
        <form method="dialog"><button class="dialog-close" aria-label="Закрыть">${icon("close", 18)}</button></form>
        <div class="dialog-icon">${icon("shield", 23)}</div><span class="eyebrow">COMMONNET DESKTOP</span><h2>Настройки клиента</h2>
        <p>Ссылка подписки хранится локально. Список обновляется при запуске и каждый час; секреты профилей остаются в памяти.</p>
        <form id="routing-form" class="routing-form">
          <label for="routing-uri">Профиль маршрутизации Incy</label>
          <textarea id="routing-uri" class="routing-uri" placeholder="incy://routing/onadd/…" required></textarea>
          <button class="primary-button" id="routing-import-button" type="submit">${icon("download", 17)}<span>Проверить и сохранить</span></button>
        </form>
        <div class="form-feedback" id="routing-feedback" aria-live="polite"></div>
        <div class="routing-summary" id="routing-summary">${escapeHtml(formatRoutingSummary(state.routingSummary))}</div>
        <p class="routing-note">Правила применяются к подключениям через Xray. Для AmneziaWG этот профиль маршрутизации не используется; новые правила начнут действовать при следующем подключении.</p>
        <div class="settings-platform"><span>Операционная система</span><strong>${escapeHtml(state.runtime?.platform ?? "Определение…")}</strong></div>
        <div class="settings-platform"><span>Xray core</span><strong>${state.runtime?.xrayDetected ? "Обнаружен" : "Не обнаружен"}</strong></div>
        <div class="settings-platform"><span>AmneziaWG tools</span><strong>${state.runtime?.awgDetected ? "Обнаружены" : "Не обнаружены"}</strong></div>
      </dialog>
    </div>`;

  bindEvents();
}

function setFeedback(message: string, type: "error" | "success" | "loading" = "error") {
  const target = document.querySelector<HTMLDivElement>("#form-feedback");
  if (!target) return;
  target.className = `form-feedback ${message ? `is-${type}` : ""}`;
  target.textContent = message;
}

function setRoutingFeedback(message: string, type: "error" | "success" | "loading" = "error") {
  const target = document.querySelector<HTMLDivElement>("#routing-feedback");
  if (!target) return;
  target.className = `form-feedback ${message ? `is-${type}` : ""}`;
  target.textContent = message;
}

function bindEvents() {
  document.querySelectorAll<HTMLButtonElement>("[data-profile-id]").forEach((button) => {
    button.addEventListener("click", () => {
      state.selectedId = button.dataset.profileId ?? null;
      persistCachedProfiles();
      render();
    });
  });

  document.querySelector<HTMLInputElement>("#subscription-url")?.addEventListener("input", (event) => {
    state.subscriptionDraft = (event.currentTarget as HTMLInputElement).value;
  });

  document.querySelector<HTMLFormElement>("#subscription-form")?.addEventListener("submit", async (event) => {
    event.preventDefault();
    const input = document.querySelector<HTMLInputElement>("#subscription-url");
    const url = input?.value.trim();
    if (!url) return;
    state.subscriptionDraft = url;
    void loadSubscription(url);
  });

  document.querySelector<HTMLFormElement>("#routing-form")?.addEventListener("submit", async (event) => {
    event.preventDefault();
    const textarea = document.querySelector<HTMLTextAreaElement>("#routing-uri");
    const button = document.querySelector<HTMLButtonElement>("#routing-import-button");
    const uri = textarea?.value.trim();
    if (!uri || !button) return;
    button.disabled = true;
    button.innerHTML = '<span class="spinner"></span><span>Проверяем профиль и геоданные…</span>';
    setRoutingFeedback("Проверяем ссылку, загружаем файлы маршрутизации и проверяем конфигурацию в Xray…", "loading");
    try {
      const summary = await invoke<RoutingSummary>("import_routing_profile", { uri });
      state.routingSummary = summary;
      const summaryNode = document.querySelector<HTMLDivElement>("#routing-summary");
      if (summaryNode) summaryNode.textContent = formatRoutingSummary(summary);
      if (textarea) textarea.value = "";
      setRoutingFeedback("Профиль сохранён. Он будет использован при следующем подключении через Xray.", "success");
    } catch (error) {
      setRoutingFeedback(String(error), "error");
    } finally {
      button.disabled = false;
      button.innerHTML = `${icon("download", 17)}<span>Проверить и сохранить</span>`;
    }
  });

  document.querySelector<HTMLButtonElement>("#refresh-list")?.addEventListener("click", () => {
    if (state.subscriptionUrl) void loadSubscription(state.subscriptionUrl);
  });
  document.querySelector<HTMLButtonElement>("#connect-button")?.addEventListener("click", async (event) => {
    const button = event.currentTarget as HTMLButtonElement;
    button.disabled = true;
    try {
      state.connection = state.connection.connected
        ? await invoke<ConnectionStatus>("disconnect_profile")
        : await invoke<ConnectionStatus>("connect_profile", { profileId: state.selectedId });
      render();
      showToast(state.connection.connected ? `Подключено: ${state.connection.name ?? "сервер"}` : "Туннель остановлен");
    } catch (error) {
      button.disabled = false;
      showToast(String(error));
    }
  });
  document.querySelector<HTMLButtonElement>("#open-settings")?.addEventListener("click", () => {
    document.querySelector<HTMLDialogElement>("#settings-dialog")?.showModal();
  });
  document.querySelector<HTMLButtonElement>(".dialog-close")?.addEventListener("click", () => {
    document.querySelector<HTMLDialogElement>("#settings-dialog")?.close();
  });
}

let refreshTimer: number | undefined;

function scheduleSubscriptionRefresh() {
  window.clearInterval(refreshTimer);
  if (!state.subscriptionUrl) return;
  refreshTimer = window.setInterval(() => {
    if (state.subscriptionUrl) void loadSubscription(state.subscriptionUrl);
  }, SUBSCRIPTION_REFRESH_INTERVAL);
}

async function loadSubscription(url: string): Promise<boolean> {
  if (state.subscriptionSyncing) return false;
  state.subscriptionSyncing = true;
  state.subscriptionUpdateFailed = false;
  render();
  setFeedback("Загружаем подписку и проверяем конфигурации…", "loading");
  let success = false;
  let feedbackMessage = "";
  let feedbackType: "error" | "success" = "error";
  try {
    const profiles = await invoke<Profile[]>("import_subscription", { url });
    state.profiles = profiles;
    state.subscriptionUrl = url;
    state.subscriptionDraft = url;
    state.profilesReady = true;
    state.lastSubscriptionUpdate = Date.now();
    state.subscriptionUpdateFailed = false;
    if (!profiles.some((profile) => profile.id === state.selectedId)) state.selectedId = profiles[0]?.id ?? null;
    persistCachedProfiles();
    scheduleSubscriptionRefresh();
    feedbackMessage = `Серверов в подписке: ${profiles.length}`;
    feedbackType = "success";
    success = true;
  } catch (error) {
    state.subscriptionUpdateFailed = true;
    feedbackMessage = String(error);
  } finally {
    state.subscriptionSyncing = false;
    render();
    setFeedback(feedbackMessage, feedbackType);
  }
  return success;
}

let toastTimeout: number | undefined;
function showToast(message: string) {
  const toast = document.querySelector<HTMLDivElement>("#toast");
  if (!toast) return;
  toast.textContent = message;
  toast.classList.add("visible");
  window.clearTimeout(toastTimeout);
  toastTimeout = window.setTimeout(() => toast.classList.remove("visible"), 2800);
}

async function start() {
  render();
  const [runtimeResult, profilesResult, connectionResult, subscriptionResult, routingResult] = await Promise.allSettled([
    invoke<RuntimeStatus>("runtime_status"),
    invoke<Profile[]>("list_profiles"),
    invoke<ConnectionStatus>("connection_status"),
    invoke<string | null>("saved_subscription_url"),
    invoke<RoutingSummary | null>("saved_routing_profile"),
  ]);
  if (runtimeResult.status === "fulfilled") {
    state.runtime = runtimeResult.value;
  } else {
    state.runtime = {
      platform: "Desktop",
      xrayDetected: false,
      awgDetected: false,
      note: "Запусти приложение в окне Commonnet Desktop.",
    };
  }
  if (profilesResult.status === "fulfilled" && profilesResult.value.length) {
    state.profiles = profilesResult.value;
    state.profilesReady = true;
    state.lastSubscriptionUpdate = Date.now();
    if (!state.profiles.some((profile) => profile.id === state.selectedId)) state.selectedId = state.profiles[0]?.id ?? null;
    persistCachedProfiles();
  }
  if (connectionResult.status === "fulfilled") state.connection = connectionResult.value;
  if (subscriptionResult.status === "fulfilled") {
    state.subscriptionUrl = subscriptionResult.value;
    state.subscriptionDraft = subscriptionResult.value;
  }
  if (routingResult.status === "fulfilled") state.routingSummary = routingResult.value;
  render();
  if (subscriptionResult.status === "fulfilled" && subscriptionResult.value) {
    scheduleSubscriptionRefresh();
    void loadSubscription(subscriptionResult.value);
  } else if (subscriptionResult.status === "rejected") {
    setFeedback(String(subscriptionResult.reason), "error");
  }
  if (routingResult.status === "rejected") setRoutingFeedback(String(routingResult.reason), "error");
}

void start();
