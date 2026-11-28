import { localize, t } from "./i18n.js";
import { downloadLabel, renderDownload } from "./ffmpeg-progress.js";
import { overlayPositionKey } from "./overlay-layout.js";
import { setupTooltips } from "./tooltips.js";

const $ = (selector) => document.querySelector(selector);
const form = $("#control-settings");
const apply = $("#control-apply");
const fields = $("#quick-fields");
const hud = $(".hud");
const widgets = [...hud.querySelectorAll("[data-widget]")];
const edge = 12;
let positions = loadPositions();
let editing = false;
let drag;
let saved;
let pending = false;
let feedback = "";

function loadPositions() {
  try {
    const value = JSON.parse(localStorage.getItem(overlayPositionKey));
    if (value && typeof value === "object") return value;
  } catch (error) {
    console.error(error);
  }
  return {};
}

localize("ru");

const time = (seconds) => [Math.floor(seconds / 60), seconds % 60]
  .map((part) => String(part).padStart(2, "0")).join(":");
const text = (selector, value) => {
  const node = $(selector);
  if (node.textContent !== value) node.textContent = value;
};
const actionLabel = (button, label) => {
  const name = t(label);
  button.setAttribute("aria-label", name);
  button.dataset.tooltip = name;
};
const send = (action, data = {}) => window.__TAURI__.core.invoke("control_action", { request: { action, ...data } });
const values = () => ({ seconds: Number(form.elements.seconds.value), fps: Number(form.elements.fps.value), export_codec: form.elements.export_codec.value });
const dirty = () => JSON.stringify(values()) !== saved;
const refresh = () => {
  apply.disabled = !saved || fields.disabled || pending || !dirty();
  const message = document.body.dataset.phase === "downloading" ? "Дождитесь установки FFmpeg." : "Остановите буфер, чтобы изменить настройки.";
  const output = $("#control-feedback");
  if (fields.disabled) output.removeAttribute("data-error");
  text("#control-feedback", t(fields.disabled ? message : feedback));
};

const clamp = (value) => Math.max(0, Math.min(1, value));
const span = (size) => Math.max(0, size - 2 * edge);
const within = (position, size, widgetSize) => Math.max(0, Math.min(position, span(size) - widgetSize));

function place(widget) {
  const position = positions[widget.dataset.widget];
  if (!Number.isFinite(position?.x) || !Number.isFinite(position?.y)) {
    widget.removeAttribute("data-positioned");
    widget.style.removeProperty("--widget-x");
    widget.style.removeProperty("--widget-y");
    return;
  }
  const x = within(clamp(position.x) * span(hud.clientWidth), hud.clientWidth, widget.offsetWidth);
  const y = within(clamp(position.y) * span(hud.clientHeight), hud.clientHeight, widget.offsetHeight);
  widget.style.setProperty("--widget-x", `${edge + x}px`);
  widget.style.setProperty("--widget-y", `${edge + y}px`);
  widget.dataset.positioned = "";
}

function move(widget, x, y) {
  positions[widget.dataset.widget] = {
    x: within(x - edge, hud.clientWidth, widget.offsetWidth) / (span(hud.clientWidth) || 1),
    y: within(y - edge, hud.clientHeight, widget.offsetHeight) / (span(hud.clientHeight) || 1),
  };
  place(widget);
}

function savePositions() {
  try {
    localStorage.setItem(overlayPositionKey, JSON.stringify(positions));
  } catch (error) {
    console.error(error);
  }
}

function finishDrag(persist = true) {
  if (!drag) return;
  const { widget, pointer } = drag;
  drag = undefined;
  widget.removeAttribute("data-dragging");
  if (widget.hasPointerCapture(pointer)) widget.releasePointerCapture(pointer);
  if (persist) savePositions();
}

function pinLabel() {
  text("#hud-pin-label", t(editing ? "Закрепить блоки" : "Переместить блоки"));
  $("#hud-lock-shackle").setAttribute("d", editing ? "M8 10V7a4 4 0 0 1 7.8-1.2" : "M8 10V7a4 4 0 0 1 8 0v3");
}

function setEditing(active) {
  finishDrag();
  editing = active;
  hud.toggleAttribute("data-editing", active);
  for (const widget of widgets) {
    if (editing) widget.tabIndex = 0;
    else widget.removeAttribute("tabindex");
  }
  pinLabel();
}

const layout = new ResizeObserver(() => widgets.forEach(place));
layout.observe(hud);
for (const widget of widgets) {
  layout.observe(widget);
  place(widget);
  widget.addEventListener("pointerdown", (event) => {
    if (!editing || drag || event.button !== 0 || event.target.closest("button, select, option, label")) return;
    const bounds = widget.getBoundingClientRect();
    drag = { widget, pointer: event.pointerId, x: event.clientX - bounds.left, y: event.clientY - bounds.top };
    widget.setPointerCapture(event.pointerId);
    widget.dataset.dragging = "";
    event.preventDefault();
  });
  widget.addEventListener("pointermove", (event) => {
    if (drag?.widget === widget && drag.pointer === event.pointerId) {
      move(widget, event.clientX - drag.x, event.clientY - drag.y);
    }
  });
  widget.addEventListener("lostpointercapture", () => {
    if (drag?.widget === widget) finishDrag();
  });
  widget.addEventListener("keydown", (event) => {
    if (!editing || event.target !== widget) return;
    const delta = { ArrowUp: [0, -8], ArrowDown: [0, 8], ArrowLeft: [-8, 0], ArrowRight: [8, 0] }[event.key];
    if (!delta) return;
    event.preventDefault();
    const bounds = widget.getBoundingClientRect();
    move(widget, bounds.left + delta[0], bounds.top + delta[1]);
    savePositions();
  });
}
$("#hud-pin").addEventListener("click", () => setEditing(!editing));

function restorePositions() {
  finishDrag(false);
  setEditing(false);
  positions = loadPositions();
  widgets.forEach(place);
}

window.addEventListener("storage", (event) => {
  if (event.storageArea === localStorage && event.key === overlayPositionKey) restorePositions();
});

window.rewind = {
  opened() {
    hud.removeAttribute("data-dismissing");
    restorePositions();
  },
  dismiss(closeId) {
    hud.dataset.dismissing = "";
    restorePositions();
    requestAnimationFrame(() => requestAnimationFrame(() => {
      send("dimming_cleared", { close_id: closeId }).catch(console.error);
    }));
  },
  status({ locale, phase, seconds, limit, saving, toggle_label, toggle_enabled, settings, download }) {
    localize(locale);
    pinLabel();
    document.body.dataset.phase = phase;
    const downloading = phase === "downloading";
    const canSave = seconds > 0 && !saving && !downloading && phase !== "stopping";
    text("#control-state", downloading ? downloadLabel(download) : t({
      idle: seconds ? "Запись остановлена" : "Буфер выключен", starting: "Запуск записи",
      recording: "Идёт запись", stopping: "Завершаем запись",
    }[phase]));
    text("#control-elapsed", time(seconds));
    text("#control-limit", "/ " + time(limit));
    const meter = $("#control-filled");
    meter.max = limit;
    meter.value = seconds;
    meter.hidden = downloading;
    const progress = $("#control-download");
    progress.hidden = !downloading;
    if (downloading) renderDownload(progress, download);
    const toggle = $("#control-toggle");
    toggle.disabled = !toggle_enabled;
    toggle.classList.toggle("primary", !canSave);
    const icon = downloading ? "cancel" : phase === "idle" ? "start" : "stop";
    for (const glyph of toggle.querySelectorAll("[data-control-icon]")) {
      glyph.toggleAttribute("hidden", glyph.dataset.controlIcon !== icon);
    }
    actionLabel(toggle, toggle_label);
    const save = $("#control-save");
    save.disabled = !canSave;
    save.classList.toggle("primary", canSave);
    actionLabel(save, saving ? "Сохраняем…" : "Сохранить повтор");
    const next = JSON.stringify(settings);
    if (next !== saved) {
      saved = next;
      for (const [name, value] of Object.entries(settings)) {
        const control = form.elements[name];
        if (![...control.options].some((option) => option.value === String(value))) {
          control.add(new Option(name === "seconds" ? time(value) : value, value));
        }
        control.value = value;
      }
    }
    fields.disabled = phase !== "idle" || saving;
    refresh();
  },
  applied(error) {
    pending = false;
    $("#control-feedback").toggleAttribute("data-error", Boolean(error));
    feedback = error ?? "Настройки сохранены.";
    refresh();
  },
};

document.querySelectorAll("[data-action]").forEach((button) => {
  button.addEventListener("click", () => send(button.dataset.action).catch(console.error));
});
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && !event.defaultPrevented && !document.querySelector("select:open")) {
    event.preventDefault();
    send("dismiss").catch(console.error);
  }
});
form.addEventListener("input", () => {
  feedback = "";
  $("#control-feedback").removeAttribute("data-error");
  refresh();
});
form.addEventListener("submit", (event) => {
  event.preventDefault();
  if (apply.disabled) return;
  pending = true;
  refresh();
  send("apply", { settings: values() }).catch((error) => window.rewind.applied(String(error)));
});
send("ready").catch(console.error);
setupTooltips();
