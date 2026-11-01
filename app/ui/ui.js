// mar-ui on top of the settings window: kit icons where the markup asks for
// them (<span data-kit-icon="name">), and their hover / press / selection
// motion. app.js does not depend on this file; without it the window works,
// only the icons are missing.
import { iconSvg } from "./vendor/mar-ui/icons.js";
import { initIconMotion } from "./vendor/mar-ui/icon-motion.js";

/** Replace every placeholder under `root` with the kit's SVG. */
export function hydrateIcons(root = document) {
  for (const slot of root.querySelectorAll("[data-kit-icon]")) {
    const svg = iconSvg(slot.dataset.kitIcon);
    if (!svg) continue;
    const template = document.createElement("template");
    template.innerHTML = svg;
    slot.replaceWith(template.content.firstElementChild);
  }
}

hydrateIcons();
initIconMotion();
// app.js re-renders some rows; icons inside them are hydrated as they appear.
new MutationObserver((records) => {
  for (const record of records) {
    for (const node of record.addedNodes) {
      if (node instanceof Element && (node.matches("[data-kit-icon]") || node.querySelector("[data-kit-icon]"))) {
        hydrateIcons(node.parentElement ?? document);
      }
    }
  }
}).observe(document.body, { childList: true, subtree: true });
