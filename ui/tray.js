import { localize, t } from "./i18n.js";

localize("ru");

const $ = (selector) => document.querySelector(selector);
const send = (action) => window.__TAURI__.core.invoke("tray_action", { action }).catch(console.error);
const time = (seconds) => [Math.floor(seconds / 60), seconds % 60]
  .map((value) => String(value).padStart(2, "0")).join(":");
const text = (selector, value) => {
  const node = $(selector);
  if (node.textContent !== value) node.textContent = value;
};

window.rewind = {
  status({ phase, seconds, limit, saving, shortcut, locale }) {
    localize(locale);
    document.body.dataset.phase = phase;
    text("#record-state", t({
      idle: seconds ? "Запись остановлена" : "Буфер выключен",
      starting: "Запуск записи",
      recording: "Идёт запись",
      stopping: "Завершаем запись",
      downloading: "Установка FFmpeg",
    }[phase]));
    text("#elapsed", time(seconds));
    text("#limit", "/ " + time(limit));
    $("#buffer").max = limit;
    $("#buffer").value = seconds;
    $("[data-action=save]").disabled = saving || phase === "downloading" || phase === "stopping" || (phase === "idle" && !seconds);
    text("#save-label", t(saving ? "Сохраняем…" : "Сохранить повтор"));
    $("#shortcut").hidden = !shortcut || saving;
    if (shortcut) text("#shortcut", "Alt + F" + shortcut);
  },
  reveal() {
    const panel = $(".tray-panel");
    if (!matchMedia("(prefers-reduced-motion: reduce)").matches) {
      panel.getAnimations().forEach((animation) => animation.cancel());
      panel.animate([{ opacity: .55, transform: "translateY(5px)" }, { opacity: 1, transform: "translateY(0)" }], { duration: 155, easing: "ease-out" });
    }
  },
};

document.querySelectorAll("[data-action]").forEach((button) => {
  button.addEventListener("click", () => send(button.dataset.action));
});
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape") {
    event.preventDefault();
    send("dismiss");
  }
});
send("ready");
