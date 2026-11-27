import { t } from "./i18n.js";

export function setupTooltips() {
  const tooltip = document.querySelector("#tooltip");
  let active;
  let timer;

  function hide() {
    clearTimeout(timer);
    if (!active) return;
    active.removeAttribute("aria-describedby");
    tooltip.hidePopover();
    active = null;
  }

  function show(target) {
    hide();
    if (!target.dataset.tooltip) return;
    active = target;
    tooltip.textContent = t(target.dataset.tooltip);
    if (tooltip.textContent !== target.getAttribute("aria-label")) {
      target.setAttribute("aria-describedby", tooltip.id);
    }
    tooltip.showPopover();
    const anchor = target.getBoundingClientRect();
    const popup = tooltip.getBoundingClientRect();
    const center = anchor.left + (anchor.width - popup.width) / 2;
    const left = Math.max(12, Math.min(center, document.documentElement.clientWidth - popup.width - 12));
    const above = anchor.top - popup.height - 7;
    tooltip.style.left = left + "px";
    tooltip.style.top = (above >= 8 ? above : anchor.bottom + 7) + "px";
  }

  for (const target of document.querySelectorAll("[data-tooltip]")) {
    target.addEventListener("pointerenter", (event) => {
      if (event.pointerType === "touch") return;
      clearTimeout(timer);
      if (active !== target) timer = setTimeout(() => show(target), 350);
    });
    target.addEventListener("pointerleave", () => {
      clearTimeout(timer);
      if (active === target && !target.matches(":focus-visible")) timer = setTimeout(hide, 150);
    });
    target.addEventListener("focus", () => {
      if (target.matches(":focus-visible")) show(target);
    });
    target.addEventListener("blur", () => { if (active === target) hide(); });
  }
  tooltip.addEventListener("pointerenter", () => clearTimeout(timer));
  tooltip.addEventListener("pointerleave", () => {
    if (!active?.matches(":focus-visible")) timer = setTimeout(hide, 150);
  });
  document.addEventListener("click", hide);
  document.addEventListener("keydown", (event) => { if (event.key === "Escape") hide(); });
  window.addEventListener("scroll", hide, { passive: true });
  window.addEventListener("resize", hide);
}
