import { language, t } from "./i18n.js";

export function downloadLabel(download) {
  switch (download?.stage) {
    case "downloading": return t("Скачиваем FFmpeg");
    case "checking": return t("Проверяем архив FFmpeg");
    case "extracting": return t("Распаковываем FFmpeg");
    default: return t(download?.mirror ? "Подключаемся к зеркалу" : "Подключаемся к серверу");
  }
}

export function renderDownload(container, download) {
  const progress = container.querySelector("progress");
  const percent = container.querySelector("[data-download-percent]");
  const detail = container.querySelector("[data-download-detail]");
  if (download?.stage !== "downloading") {
    progress.removeAttribute("value");
    percent.hidden = true;
    detail.textContent = "";
    return;
  }
  const value = Math.floor(download.bytes / download.total * 100);
  const megabytes = new Intl.NumberFormat(language(), { maximumFractionDigits: 1, minimumFractionDigits: 1 });
  const size = (bytes) => megabytes.format(bytes / 1_000_000);
  percent.hidden = false;
  percent.textContent = value + "%";
  progress.value = value;
  const seconds = download.remaining;
  const time = seconds == null ? t("Ожидаем данные…") :
    `≈ ${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, "0")} ${t("осталось")}`;
  detail.textContent = `${size(download.bytes)} / ${size(download.total)} ${t("МБ")} · ${time}`;
}
