import { language, localize, t } from "./i18n.js";
import { setupTooltips } from "./tooltips.js";
import { downloadLabel, renderDownload } from "./ffmpeg-progress.js";
import { overlayPositionKey } from "./overlay-layout.js";
import { renderMicrophone } from "./microphone.js";

const icons = {
  rewind: '<path fill="currentColor" stroke="none" d="M11 5v14L2 12l9-7Zm10 0v14l-9-7 9-7Z"/>',
  play: '<path fill="currentColor" stroke="none" d="m8 4 12 8-12 8V4Z"/>',
  folder: '<path d="M3 6h6l2 2h10v11H3z"/>',
  monitor: '<rect x="3" y="4" width="18" height="13" rx="1"/><path d="M12 17v4m-4 0h8"/>',
  overlay: '<rect x="3" y="4" width="18" height="16" rx="2"/><rect x="11" y="11" width="7" height="6" rx="1"/>',
  layers: '<path d="m12 3 9 5-9 5-9-5 9-5Zm-9 9 9 5 9-5m-18 4 9 5 9-5"/>',
  chip: '<rect x="5" y="5" width="14" height="14" rx="1"/><path d="M9 2v3m6-3v3M9 19v3m6-3v3M2 9h3m-3 6h3m14-6h3m-3 6h3"/>',
  wave: '<path d="M4 10v4m4-8v12m4-15v18m4-14v10m4-7v4"/>',
  chevron: '<path d="m9 4 8 8-8 8"/>',
  film: '<rect x="3" y="3" width="18" height="18" rx="1"/><path d="M7 3v18M17 3v18M3 9h4m-4 6h4m10-6h4m-4 6h4M7 12h10"/>',
  alert: '<path d="m12 3 10 17H2L12 3Zm0 6v4m0 3h.01"/>',
  refresh: '<path d="M20 7v5h-5m5 0a8 8 0 1 0-2 6"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  minimize: '<path d="M5 12h14"/>',
  maximize: '<rect x="5" y="5" width="14" height="14" rx="1"/>',
  close: '<path d="m6 6 12 12M18 6 6 18"/>',
};

const $ = (selector) => document.querySelector(selector);
const all = (selector) => [...document.querySelectorAll(selector)];
const field = (name) => $("[name=" + name + "]");
const preferenceNames = ["language", "close_behavior", "interface_scale"];
const onText = (selector, value) => {
  const element = $(selector);
  if (element.textContent !== value) element.textContent = value;
};
const ipc = (action, body = {}) => window.__TAURI__.core.invoke("dispatch", { request: { action, ...body } })
  .catch((error) => {
    console.error(error);
    window.rewind?.rejected();
  });

function icon(element, name = element.dataset.icon) {
  if (element.firstChild && element.dataset.icon === name) return;
  element.dataset.icon = name;
  element.innerHTML = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.65" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">' + icons[name] + '</svg>';
}
all("[data-icon]").forEach((element) => icon(element));

let settings;
let ffmpegEdited = false;
let applications = [];
let update = { state: "idle", version: null };
let releaseHistory = [];
let unreadRelease = null;
let shortcut = true;
let saving = false;
let phase = "idle";
const defaultOverlayShortcut = { key: 90, ctrl: false, alt: true, shift: false, win: false };
const shortcutKeys = { Space: 32, PageUp: 33, PageDown: 34, End: 35, Home: 36, ArrowLeft: 37, ArrowUp: 38, ArrowRight: 39, ArrowDown: 40, Insert: 45, Delete: 46 };
let capturingShortcut = false;
let overlayBinding = { shortcut: defaultOverlayShortcut, registered: false };

function showOverlayShortcut() {
  const { shortcut, registered, error } = overlayBinding;
  const { key, ctrl, alt, shift, win } = shortcut;
  const known = Object.keys(shortcutKeys).find((name) => shortcutKeys[name] === key);
  const label = key >= 112 ? `F${key - 111}` : known?.replace(/^Arrow/, "") ?? String.fromCharCode(key);
  const combination = [ctrl && "Ctrl", alt && "Alt", shift && "Shift", win && "Win", label].filter(Boolean).join(" + ");
  onText("#overlay-shortcut", capturingShortcut ? t("Нажми сочетание…") : combination);
  onText("#overlay-shortcut-status", capturingShortcut ? t("Нажми Esc для отмены.") : (error ?? (registered ? "" : t("Горячая клавиша оверлея занята или недоступна."))));
  const open = $("#open-overlay");
  open.dataset.tooltip = registered ? `${t("Открыть оверлей Rewind")} (${combination})` : "Открыть оверлей Rewind";
  if (registered && !win) open.setAttribute("aria-keyshortcuts", combination.replaceAll(" ", ""));
  else open.removeAttribute("aria-keyshortcuts");
}
let last = null;
let limit = 0;
let page = "buffer";
let dirty = false;
let applying = false;
let browsing = false;
let pendingAction = false;
let initialized = false;
let lastStatus;
let cycle = null;
let lastRecorded = 0;
let plotStart = 0;
const levels = new Map();

const ticks = Array.from({ length: 100 }, () => document.createElement("i"));
$("#ticks").append(...ticks);

function time(seconds) {
  seconds = Math.floor(seconds);
  return [Math.floor(seconds / 60), seconds % 60].map((part) => String(part).padStart(2, "0")).join(":");
}

function duration(seconds) {
  if (language() === "en") {
    if (seconds % 60 !== 0) return `${seconds} seconds`;
    const n = seconds / 60;
    return n === 1 ? "1 minute" : `${n} minutes`;
  }
  if (seconds % 60 !== 0) return seconds + " сек.";
  const n = seconds / 60;
  return n + " " + ({ one: "минуту", few: "минуты", many: "минут" })[new Intl.PluralRules("ru").select(n)];
}

function updateScale(maximum) {
  onText("#intro", language() === "en"
    ? `Save the last ${duration(maximum)} of screen and audio.`
    : `Сохраняйте последние ${duration(maximum)} экрана и звука.`);
  $("#scale").replaceChildren(...[1, .75, .5, .25, 0].map((ratio) => {
    const tick = document.createElement("span");
    tick.textContent = ratio ? "−" + time(maximum * ratio) : t("Сейчас");
    return tick;
  }));
}
function goto(next, focus = false) {
  if (next === page) return;
  page = next;
  all(".page").forEach((section) => { section.hidden = section.id !== next; });
  all(".tab").forEach((tab) => {
    if (tab.dataset.page === next) tab.setAttribute("aria-current", "page");
    else tab.removeAttribute("aria-current");
  });
  if (focus) {
    const heading = $("#" + next + " h1");
    heading.tabIndex = -1;
    heading.focus({ preventScroll: true });
  }
  window.scrollTo(0, 0);
  if (!matchMedia("(prefers-reduced-motion: reduce)").matches) {
    $("#" + next).animate([{ opacity: .65, transform: "translateY(4px)" }, { opacity: 1, transform: "translateY(0)" }], { duration: 155, easing: "ease-out" });
  }
}

function addTrack(track, animate = false) {
  const row = document.createElement("div");
  row.className = animate ? "track new" : "track";
  row.innerHTML = '<input name="track_name" type="text" maxlength="48" required><label><input class="switch" type="checkbox" name="track_system"><span></span></label><label><input class="switch" type="checkbox" name="track_microphone"><span></span></label><select name="track_application"></select><button type="button" class="pill remove"><span data-icon="close"></span></button>';
  labelTrack(row);
  const name = row.querySelector("[name=track_name]");
  name.value = track.name;
  row.querySelector("[name=track_system]").checked = track.system;
  row.querySelector("[name=track_microphone]").checked = track.microphone;
  listApplications(row.querySelector("[name=track_application]"), track.application ?? "");
  updateTrackSources(row);
  icon(row.querySelector("[data-icon]"));
  row.querySelector(".remove").addEventListener("click", () => {
    row.remove();
    edit();
    refreshControls();
    $("#add-track").focus();
  });
  $("#tracks").append(row);
  if (animate) name.focus();
}

function labelTrack(row) {
  row.querySelector("[name=track_name]").setAttribute("aria-label", t("Название дорожки"));
  row.querySelector("[name=track_system] + span").textContent = t("Система");
  row.querySelector("[name=track_microphone] + span").textContent = t("Микрофон");
  row.querySelector("[name=track_application]").setAttribute("aria-label", t("Приложение"));
  row.querySelector(".remove").setAttribute("aria-label", t("Удалить дорожку"));
}

function updateTrackSources(row) {
  const system = row.querySelector("[name=track_system]");
  const microphone = row.querySelector("[name=track_microphone]");
  const application = row.querySelector("[name=track_application]");
  system.disabled = microphone.disabled = Boolean(application.value);
  application.disabled = system.checked || microphone.checked;
}

function listApplications(select, current = select.value) {
  const options = [new Option(t("Без приложения"), "")];
  for (const app of applications) options.push(new Option(app.name, app.path));
  if (current && !applications.some((app) => app.path === current)) {
    options.push(new Option(current.split(/[\\/]/).at(-1) + " · " + t("Закрыто"), current));
  }
  select.replaceChildren(...options);
  select.value = current;
}

const numeric = ["width", "height", "fps", "bitrate", "seconds", "shortcut", "audio_bitrate", "system_volume", "microphone_volume"];
const strings = ["encoder", "mode", "export_codec", "output", "ffmpeg", "system_device", "microphone_device"];
const encoders = { cpu: "CPU", nvidia: "NVENC", amd: "AMF", intel: "Quick Sync" };
const locked = () => !settings || phase !== "idle" || saving || applying || pendingAction;

function setOption(control, value, label) {
  if (![...control.options].some((option) => option.value === String(value))) {
    const option = new Option(t(label), value);
    option.dataset.message = label;
    control.add(option);
  }
  control.value = value;
}

function fill(config, displays) {
  for (const name of [...numeric, ...strings]) {
    const control = field(name);
    if (name.endsWith("_device")) setOption(control, config[name], "Сохранённое устройство");
    else control.value = config[name];
  }
  field("cursor").checked = config.cursor;
  field("native_resolution").checked = config.native_resolution;
  field("automatic_capture").checked = config.automatic_capture;
  field("dxgi_display").value = config.mode === "dxgi" ? config.display : 0;
  const gdi = field("gdi_display");
  gdi.replaceChildren(...displays.map((display, index) => {
    const option = new Option(display.label, index);
    if (display.primary) {
      option.dataset.primary = display.label;
      option.textContent += " · " + t("Основной");
    }
    return option;
  }));
  const chosen = config.automatic_capture || config.mode !== "dxgi" ? config.display : 0;
  setOption(gdi, chosen, "Монитор недоступен");
  $("#tracks").replaceChildren();
  for (const track of config.tracks) addTrack(track);
  mode();
  resolutionMode();
}

function displayPath(path) {
  const extended = "\\\\?\\";
  if (path.startsWith(extended + "UNC\\")) return "\\\\" + path.slice(8);
  if (path.startsWith(extended) && /^[a-z]:\\/i.test(path.slice(4))) return path.slice(4);
  return path;
}

function mode() {
  const automatic = field("automatic_capture").checked;
  const dxgi = !automatic && field("mode").value === "dxgi";
  $("#mode-field").hidden = automatic;
  $("#encoder-field").hidden = automatic;
  $("#dxgi-field").hidden = !dxgi;
  $("#gdi-field").hidden = dxgi;
}

function resolutionMode() {
  const native = field("native_resolution").checked;
  for (const name of ["width", "height"]) {
    $("#" + name + "-field").hidden = native;
    if (native) field(name).removeAttribute("aria-invalid");
  }
}

function storage() {
  const video = Number(field("bitrate").value);
  const audio = Number(field("audio_bitrate").value);
  const count = activeTracks(trackValues(), field("system_device").value, field("microphone_device").value);
  const seconds = Number(field("seconds").value);
  const mb = (video + count * audio) * seconds / 8000;
  const size = Math.ceil(mb).toLocaleString(language());
  onText("#storage", mb <= 0 ? "" : language() === "en"
    ? `About ${size} MB of buffer space at this bitrate.`
    : `Около ${size} МБ в буфере при заданном битрейте.`);
}

function activeTracks(tracks, system, mic) {
  return tracks.filter((track) => (track.system && system) || (track.microphone && mic) || track.application).length;
}

function trackValues() {
  return all(".track").map((row) => ({
    name: row.querySelector("[name=track_name]").value,
    system: row.querySelector("[name=track_system]").checked,
    microphone: row.querySelector("[name=track_microphone]").checked,
    application: row.querySelector("[name=track_application]").value || null,
  }));
}

function feedback(text) {
  all(".feedback").forEach((element) => { if (element.textContent !== text) element.textContent = text; });
}

function refreshControls() {
  const disabled = locked();
  all(".editable").forEach((group) => { group.disabled = disabled; });
  all(".apply").forEach((button) => { button.disabled = disabled || !dirty; });
  $("#add-track").disabled = disabled || $("#tracks").childElementCount >= 10;
  $("#install-update").disabled = phase !== "idle" || saving || pendingAction;
  if (phase === "downloading") feedback(t("Дождитесь установки FFmpeg."));
  else if (phase !== "idle" || saving) feedback(t("Остановите буфер и дождитесь сохранения клипа, чтобы изменить настройки."));
}

function showUpdate() {
  if (!document.querySelector("dialog[open]")) $("#update-prompt").showModal();
}

function showReleaseNotes(version = null) {
  if (document.querySelector("dialog[open]")) return;
  const releases = version ? releaseHistory.filter((release) => release.version === version) : releaseHistory;
  if (!releases.length) return;

  const list = $("#release-notes-list");
  list.replaceChildren(...releases.map(({ version, changes }) => {
    const section = document.createElement("section");
    const heading = document.createElement("h3");
    heading.textContent = "v" + version;
    const bullets = document.createElement("ul");
    for (const change of changes) {
      const item = document.createElement("li");
      item.textContent = t(change).replace(/`([^`]+)`/g, "$1");
      bullets.append(item);
    }
    section.append(heading, bullets);
    return section;
  }));
  onText("#release-notes-title", t(version ? "Что нового" : "История версий"));
  const dialog = $("#release-notes");
  dialog.dataset.version = version ?? "";
  dialog.showModal();
  list.scrollTop = 0;
}

function showUnreadRelease() {
  if (unreadRelease && !document.hidden) showReleaseNotes(unreadRelease);
}

function refreshUpdate() {
  const { state, version, progress } = update;
  const downloadReady = state === "available" || state === "download_error";
  const downloading = state === "downloading";
  const downloadStage = { connecting: "Подключаемся к GitHub…", checking: "Проверяем файл обновления…" }[progress?.stage];
  const message = {
    checking: "Проверяем обновления…",
    current: "У вас последняя версия.",
    available: "Новая версия Rewind",
    downloading: downloadStage ?? "Скачиваем обновление…",
    downloaded: "Обновление загружено. Перед установкой сохраните нужный повтор и остановите запись.",
    check_error: "Не удалось проверить обновления.",
    download_error: "Не удалось загрузить или установить обновление.",
  }[state];
  onText("#update-status", message ? t(message) + (state === "available" ? " v" + version : "") : "");
  $("#update-status").hidden = !message;
  $("#check-update").disabled = ["checking", "downloading", "downloaded"].includes(state);
  $("#show-update").hidden = !downloadReady;
  $("#install-update").hidden = state !== "downloaded";
  onText("#update-version", version ? "v" + version : "");
  const meter = $("#update-progress");
  meter.hidden = !downloading;
  if (downloading) renderDownload(meter, progress);
}

function edit(event) {
  event?.target.removeAttribute("aria-invalid");
  dirty = true;
  feedback(t("Есть несохранённые изменения."));
  storage();
  refreshControls();
}

function setupNumberFields() {
  for (const input of all('input[type="number"]')) {
    const label = input.parentElement;
    const field = document.createElement("div");
    field.className = "number-control";
    if (label.id) {
      field.id = label.id;
      label.removeAttribute("id");
    }
    field.hidden = label.hidden;
    label.hidden = false;
    input.id = "number-" + input.name;
    label.htmlFor = input.id;

    const stepper = document.createElement("div");
    stepper.className = "number-field";
    for (const [symbol, description, method] of [
      ["−", "Уменьшить значение", "stepDown"],
      ["+", "Увеличить значение", "stepUp"],
    ]) {
      const button = document.createElement("button");
      button.type = "button";
      button.textContent = symbol;
      button.setAttribute("aria-label", description);
      button.addEventListener("click", () => {
        const previous = input.value;
        input[method]();
        if (input.value !== previous) input.dispatchEvent(new Event("input", { bubbles: true }));
      });
      stepper.append(button);
    }
    stepper.insertBefore(input, stepper.lastElementChild);
    label.replaceWith(field);
    field.append(label, stepper);
  }
}

function apply() {
  if (locked()) return;
  for (const control of all(".editable input, .editable select")) {
    if (control.closest("[hidden]")?.id.endsWith("-field")) continue;
    if (!control.checkValidity()) {
      goto(control.closest(".page").id);
      control.setAttribute("aria-invalid", "true");
      control.focus();
      control.reportValidity();
      return;
    }
  }
  const next = {
    ...Object.fromEntries(numeric.map((name) => [name, Number(field(name).value)])),
    ...Object.fromEntries(strings.map((name) => [name, field(name).value.trim()])),
    automatic_capture: field("automatic_capture").checked,
    display: Number(field($("#dxgi-field").hidden ? "gdi_display" : "dxgi_display").value),
    cursor: field("cursor").checked,
    native_resolution: field("native_resolution").checked,
    tracks: trackValues(),
  };
  if (!ffmpegEdited) next.ffmpeg = settings.ffmpeg;
  applying = true;
  feedback(t("Сохраняем настройки…"));
  refreshControls();
  ipc("apply", { settings: next });
}

async function pickPath(kind) {
  if (browsing) return null;
  browsing = true;
  try {
    return await window.__TAURI__.core.invoke("browse_path", { kind, current: field(kind).value.trim() });
  } finally {
    browsing = false;
  }
}

async function browsePath(kind) {
  if (locked()) return;
  const input = field(kind);
  try {
    const path = await pickPath(kind);
    if (path && !locked() && (path !== input.value || kind === "ffmpeg")) {
      input.value = path;
      input.dispatchEvent(new Event("input", { bubbles: true }));
    }
  } catch (error) {
    console.error(error);
    feedback(t("Не удалось открыть диалог выбора."));
  }
}

function plot(state, maximum) {
  const { seconds, level } = state;
  const recorded = state.recorded ?? 0;
  const count = Math.min(ticks.length, maximum);
  if (state.cycle !== cycle || maximum !== limit) {
    levels.clear();
    cycle = state.cycle;
    lastRecorded = recorded;
    plotStart = Math.max(0, recorded - seconds);
  }
  const current = Math.floor(Math.max(0, recorded - plotStart) * count / maximum);
  const oldest = current - count + 1;
  if (level > 0) levels.set(current, Math.max(level, levels.get(current) ?? 0));
  for (const bucket of levels.keys()) {
    if (bucket < oldest) levels.delete(bucket);
    else break;
  }
  if (recorded > lastRecorded && phase === "recording" && !matchMedia("(prefers-reduced-motion: reduce)").matches) {
    $(".playhead").animate([{ opacity: .45 }, { opacity: 1 }], { duration: 420, easing: "ease-out" });
  }
  lastRecorded = recorded;
  onText("#elapsed", time(Math.min(seconds, maximum)));
  onText("#limit", "/ " + time(maximum));
  if (maximum !== limit) {
    limit = maximum;
    $("#ticks").style.setProperty("--tick-count", count);
    ticks.forEach((tick, index) => { tick.hidden = index >= count; });
    $("#plot").setAttribute("aria-valuemax", String(maximum));
    updateScale(maximum);
  }
  const filled = seconds > 0 ? Math.min(count, Math.floor(seconds * count / maximum) + 1) : 0;
  const position = String(Math.min(Math.floor(seconds), maximum));
  if ($("#plot").getAttribute("aria-valuenow") !== position) $("#plot").setAttribute("aria-valuenow", position);
  for (let index = 0; index < count; index++) {
    const tick = ticks[index];
    const active = index >= count - filled;
    tick.classList.toggle("filled", active);
    const peak = active ? levels.get(oldest + index) ?? 0 : 0;
    const height = peak ? Math.max(0, (1 + Math.log10(peak / 32768) / 3) * 100).toFixed(1) + "%" : "0%";
    if (height !== tick.style.getPropertyValue("--tick-height")) tick.style.setProperty("--tick-height", height);
  }
}

window.rewind = {
  preferences({ preferences, locale, tray, version }) {
    for (const name of preferenceNames) field(name).value = preferences[name];
    field("start_with_windows").checked = preferences.start_with_windows;
    field("auto_update").checked = preferences.auto_update;
    field("close_behavior").querySelector('option[value="tray"]').disabled = !tray;
    onText("#app-version", "v" + version);
    if (locale === language()) return;
    localize(locale);
    all(".track").forEach(labelTrack);
    all("[name=track_application]").forEach((select) => listApplications(select));
    refreshUpdate();
    showOverlayShortcut();
    all("option[data-message]").forEach((option) => { option.textContent = t(option.dataset.message); });
    all("option[data-primary]").forEach((option) => { option.textContent = option.dataset.primary + " · " + t("Основной"); });
    if (settings) {
      onText("#summary-size", settings.native_resolution ? t("Как у экрана") : settings.width + " × " + settings.height);
      storage();
    }
    if (!last) onText("#last-file", t("Сохранённые повторы появятся здесь"));
    feedback(dirty ? t("Есть несохранённые изменения.") : "");
    if (limit) updateScale(limit);
    if (lastStatus) this.status(lastStatus);
  },
  preferencesSaved() {
    onText("#preferences-feedback", t("Настройки сохранены."));
  },
  applications(list) {
    applications = list;
    all("[name=track_application]").forEach((select) => listApplications(select));
  },
  updateState(result) {
    update = result;
    refreshUpdate();
    if (result.state === "available") showUpdate();
  },
  releaseNotes({ history, unread }) {
    releaseHistory = history;
    unreadRelease = unread;
    $("#open-release-notes").disabled = !history.length;
    showUnreadRelease();
  },
  preferencesSaveError() {
    onText("#preferences-feedback", t("Не удалось сохранить настройки"));
  },
  closePrompt(tray) {
    const dialog = $("#close-prompt");
    if (dialog.open) return;
    dialog.returnValue = "";
    field("remember_close").checked = false;
    dialog.querySelector('[value="tray"]').disabled = !tray;
    dialog.showModal();
  },
  ffmpegPrompt() {
    const dialog = $("#ffmpeg-prompt");
    if (dialog.open) return;
    pendingAction = true;
    refreshControls();
    onText("#ffmpeg-prompt-error", "");
    dialog.showModal();
  },
  ffmpegPath(path) {
    settings.ffmpeg = path;
    ffmpegEdited = false;
    this.detectedFfmpeg(path);
  },
  detectedFfmpeg(path) {
    if (!ffmpegEdited) field("ffmpeg").value = displayPath(path ?? settings?.ffmpeg ?? "");
  },
  overlayShortcut(binding) {
    overlayBinding = binding;
    capturingShortcut = false;
    showOverlayShortcut();
  },
  quickSettings(quick) {
    if (!settings) return;
    for (const [name, value] of Object.entries(quick)) {
      const control = field(name);
      if (control.value === String(settings[name])) control.value = value;
      settings[name] = value;
    }
    storage();
  },
  configure(config) {
    settings = config.settings;
    shortcut = config.shortcut;
    fill(settings, config.displays);
    ffmpegEdited = false;
    this.detectedFfmpeg(config.detected_ffmpeg);
    storage();
    onText("#function-key", "F" + settings.shortcut);
    onText("#summary-size", settings.native_resolution ? t("Как у экрана") : settings.width + " × " + settings.height);
    onText("#summary-fps", settings.fps + " FPS");
    onText("#summary-encoder", settings.automatic_capture ? t("Автовыбор") : "H.264 · " + encoders[settings.encoder]);
    onText("#summary-tracks", String(activeTracks(settings.tracks, settings.system_device, settings.microphone_device)));
    refreshControls();
  },
  devices([outputs, inputs]) {
    $("#device-error").hidden = true;
    for (const [name, devices] of [["system_device", outputs], ["microphone_device", inputs]]) {
      const control = field(name);
      const current = control.value;
      control.replaceChildren(...[...control.options].filter((option) => option.value === "" || option.value === "default"));
      control.append(...devices.map((device) => new Option(device.name, device.id)));
      setOption(control, current, "Устройство отключено или недоступно");
    }
  },
  deviceError(error) {
    onText("#device-error", t("Не удалось обновить устройства.") + " " + error);
    $("#device-error").hidden = false;
  },
  applied() {
    applying = false;
    dirty = false;
    feedback(t("Настройки сохранены."));
    refreshControls();
  },
  rejected() {
    pendingAction = false;
    if (applying) {
      applying = false;
      feedback(t("Исправьте ошибку и примените настройки снова."));
    }
    refreshControls();
  },
  status(state) {
    lastStatus = state;
    renderMicrophone(state.microphone_muted);
    phase = state.phase;
    saving = state.saving;
    if (phase !== "idle" || saving || state.error) pendingAction = false;
    const isIdle = phase === "idle";
    const downloading = phase === "downloading";
    const running = !isIdle && phase !== "stopping" && !downloading;
    document.body.dataset.phase = phase;
    if (settings) {
      const fps = state.actual_fps == null ? settings.fps : state.actual_fps + " / " + settings.fps;
      onText("#summary-fps", fps + " FPS");
      const encoder = !isIdle && state.encoder ? state.encoder : settings.automatic_capture ? null : settings.encoder;
      const mode = !isIdle && state.capture_mode ? state.capture_mode.toUpperCase() + " · " : "";
      onText("#summary-encoder", encoder ? mode + encoders[encoder] : t("Автовыбор"));
    }
    const installLabel = downloading ? downloadLabel(state.download) : "";
    onText("#global-state", downloading ? installLabel : running ? t("Буфер включён") : t("Буфер выключен"));
    const recording = t("Идёт запись") + " · " + time(state.recorded ?? 0);
    onText("#local-state", { idle: state.seconds ? t("Запись остановлена") : t("Ожидает запуска"), recording, starting: t("Запуск записи"), stopping: t("Завершаем запись"), downloading: installLabel }[phase]);
    plot(state, state.limit ?? settings?.seconds ?? 300);
    const progress = $("#ffmpeg-progress");
    progress.hidden = !downloading;
    $("#plot").hidden = $("#scale").hidden = downloading;
    if (downloading) renderDownload(progress, state.download);
    const primary = $("#primary");
    const save = (!isIdle && !downloading) || Boolean(state.seconds);
    primary.dataset.command = save ? "save" : "start";
    primary.disabled = !settings || saving || pendingAction || phase === "stopping" || downloading;
    icon($("#primary-icon"), save ? "rewind" : "play");
    $("#primary-icon").hidden = downloading;
    onText("#primary-text", downloading ? installLabel : saving ? t("Сохраняем…") : save ? t("Сохранить повтор") : t("Включить буфер"));
    $("#stop").hidden = isIdle && !state.seconds;
    $("#stop").disabled = phase === "stopping" || (isIdle && saving);
    $("#stop").dataset.action = isIdle ? "start" : "stop";
    onText("#stop", downloading ? t("Отменить") : isIdle ? t("Начать заново") : t("Остановить"));
    $("#keys").hidden = !shortcut;
    $("#shortcut-warning").hidden = shortcut;
    $("#error").hidden = !state.error;
    onText("#error-text", state.error ?? "");
    if (state.last !== last || !initialized) {
      $("#open-clip").hidden = !state.last;
      onText("#last-file", state.last?.split(/[\\/]/).at(-1) ?? t("Сохранённые повторы появятся здесь"));
      $("#last-file").dataset.tooltip = state.last ?? "";
      if (state.last) $("#last-file").tabIndex = 0;
      else $("#last-file").removeAttribute("tabindex");
      if (initialized && state.last) {
        const latest = $("#latest");
        latest.classList.remove("saved");
        void latest.offsetWidth;
        latest.classList.add("saved");
      }
      last = state.last;
      initialized = true;
    }
    refreshControls();
    if (isIdle && !saving && [t("Остановите буфер и дождитесь сохранения клипа, чтобы изменить настройки."), t("Дождитесь установки FFmpeg.")].includes($(".feedback").textContent)) {
      feedback(dirty ? t("Есть несохранённые изменения.") : "");
    }
  },
};

document.addEventListener("click", (event) => {
  const button = event.target.closest("button");
  if (!button || button.disabled) return;
  if (button.dataset.window) {
    const current = window.__TAURI__.window.getCurrentWindow();
    current[button.dataset.window]().catch(console.error);
    return;
  }
  if (button.dataset.page) goto(button.dataset.page, !button.matches(".tab"));
  if (button.dataset.action) ipc(button.dataset.action);
  if (button.dataset.browse) void browsePath(button.dataset.browse);
  if (button.id === "show-update") showUpdate();
  if (button.id === "primary" && button.dataset.command) {
    pendingAction = true;
    button.disabled = true;
    ipc(button.dataset.command);
  }
  if (button.matches(".apply")) apply();
  if (button.id === "add-track") {
    addTrack({ name: t("Дорожка") + " " + ($("#tracks").childElementCount + 1), system: false, microphone: false }, true);
    edit();
  }
});
document.addEventListener("input", (event) => {
  if (event.target === field("ffmpeg")) ffmpegEdited = true;
  const row = event.target.closest(".track");
  if (row) updateTrackSources(row);
  if (event.target.closest(".editable")) edit(event);
});
$("#update-prompt").addEventListener("submit", (event) => {
  event.preventDefault();
  $("#update-prompt").close();
  if (event.submitter?.value === "download") {
    goto("settings");
    $("#updates").scrollIntoView({ block: "nearest" });
    ipc("download_update");
  }
});
$("#open-release-notes").addEventListener("click", () => showReleaseNotes());
$("#release-notes").addEventListener("close", (event) => {
  if (event.target.dataset.version) {
    unreadRelease = null;
    ipc("read_release_notes");
  }
});
all("dialog").forEach((dialog) => dialog.addEventListener("close", showUnreadRelease));
document.addEventListener("visibilitychange", showUnreadRelease);
document.querySelector('.tab[data-page="audio"]').addEventListener("click", () => ipc("refresh_applications"));
field("mode").addEventListener("change", mode);
field("automatic_capture").addEventListener("change", mode);
field("native_resolution").addEventListener("change", resolutionMode);
const overlayShortcutButton = $("#overlay-shortcut");
function captureOverlayShortcut(enabled) {
  capturingShortcut = enabled;
  showOverlayShortcut();
  ipc("capture_overlay_shortcut", { enabled });
}
overlayShortcutButton.addEventListener("click", () => captureOverlayShortcut(!capturingShortcut));
overlayShortcutButton.addEventListener("blur", () => {
  if (capturingShortcut) captureOverlayShortcut(false);
});
overlayShortcutButton.addEventListener("keydown", (event) => {
  if (!capturingShortcut || event.key === "Tab") return;
  event.preventDefault();
  if (event.key === "Escape") {
    captureOverlayShortcut(false);
    return;
  }
  const { code, altKey: alt, ctrlKey: ctrl, shiftKey: shift, metaKey: win } = event;
  let key = shortcutKeys[code];
  if (/^Key[A-Z]$/.test(code)) key = code.charCodeAt(3);
  if (/^Digit[0-9]$/.test(code)) key = code.charCodeAt(5);
  if (/^F(?:[1-9]|1[0-9]|2[0-4])$/.test(code)) key = 111 + Number(code.slice(1));
  if (event.repeat || !key || !(alt || ctrl || shift || win)) return;
  capturingShortcut = false;
  ipc("overlay_shortcut", { shortcut: { key, alt, ctrl, shift, win } });
});
$("#reset-overlay-shortcut").addEventListener("click", () => ipc("overlay_shortcut", { shortcut: defaultOverlayShortcut }));
$("#reset-overlay-layout").addEventListener("click", () => {
  try {
    localStorage.removeItem(overlayPositionKey);
    onText("#preferences-feedback", t("Расстановка оверлея сброшена."));
  } catch (error) {
    console.error(error);
    onText("#preferences-feedback", t("Не удалось сбросить расстановку оверлея."));
  }
});
for (const name of [...preferenceNames, "start_with_windows", "auto_update"]) {
  field(name).addEventListener("change", () => {
    onText("#preferences-feedback", t("Сохраняем настройки…"));
    ipc("preferences", { preferences: {
      language: field("language").value,
      close_behavior: field("close_behavior").value,
      interface_scale: Number(field("interface_scale").value),
      start_with_windows: field("start_with_windows").checked,
      auto_update: field("auto_update").checked,
    } });
  });
}
$("#close-prompt form").addEventListener("submit", (event) => {
  event.preventDefault();
  $("#close-prompt").close(event.submitter?.value ?? "cancel");
});
$("#close-prompt").addEventListener("close", (event) => {
  const behavior = event.target.returnValue;
  if (behavior === "tray" || behavior === "exit") {
    ipc("close_choice", { behavior, remember: field("remember_close").checked });
  }
});
const ffmpegPrompt = $("#ffmpeg-prompt");
ffmpegPrompt.addEventListener("submit", async (event) => {
  event.preventDefault();
  if (browsing) return;
  const choice = event.submitter.value;
  let path;
  if (choice === "path") {
    onText("#ffmpeg-prompt-error", "");
    try {
      path = await pickPath("ffmpeg");
    } catch (error) {
      console.error(error);
      onText("#ffmpeg-prompt-error", t("Не удалось открыть диалог выбора."));
      return;
    }
    if (!path || !ffmpegPrompt.open) return;
  }
  ffmpegPrompt.close();
  ipc("ffmpeg_choice", { choice, path });
});
ffmpegPrompt.addEventListener("cancel", () => {
  pendingAction = false;
  ipc("ffmpeg_choice", { choice: "cancel" });
});
setupNumberFields();
localize("ru");
setupTooltips();
ipc("ready");
