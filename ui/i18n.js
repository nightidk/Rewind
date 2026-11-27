const english = await fetch("/strings-en.json").then((response) => response.json());
let locale = "ru";
let strings;

export function t(message) {
  return locale === "en" ? english[message] ?? message : message;
}

export function language() {
  return locale;
}

export function localize(next) {
  if (!strings) {
    strings = [];
    const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
    while (walker.nextNode()) {
      const node = walker.currentNode;
      const key = node.data.trim();
      if (Object.hasOwn(english, key)) {
        const original = node.data;
        strings.push(() => { node.data = original.replace(key, t(key)); });
      }
    }
    for (const element of document.querySelectorAll("[aria-label], [title], [placeholder]")) {
      for (const attribute of ["aria-label", "title", "placeholder"]) {
        const key = element.getAttribute(attribute);
        if (Object.hasOwn(english, key)) {
          strings.push(() => element.setAttribute(attribute, t(key)));
        }
      }
    }
    const title = document.title;
    strings.push(() => { document.title = t(title); });
  }
  next = next === "en" ? "en" : "ru";
  if (locale === next) return;
  locale = next;
  document.documentElement.lang = locale;
  strings.forEach((translate) => translate());
}
