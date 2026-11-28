import { t } from "./i18n.js";

const button = document.querySelector(".mic-toggle");
button.innerHTML = `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
  <rect x="9" y="3" width="6" height="12" rx="3"/>
  <path d="M5 11v1a7 7 0 0 0 14 0v-1m-7 8v3m-3 0h6"/>
  <path class="mic-slash" d="m3 3 18 18"/>
</svg>`;

export function renderMicrophone(muted) {
  button.disabled = false;
  button.setAttribute("aria-pressed", String(muted));
  button.setAttribute("aria-label", t("Заглушить микрофон"));
  button.dataset.tooltip = muted ? "Включить микрофон" : "Заглушить микрофон";
}
