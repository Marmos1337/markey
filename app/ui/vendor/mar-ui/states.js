/* mar-ui 1.0.0 7af02f8 — vendored by mar-ui/tools/vendor.mjs; do not edit here, update the kit and re-vendor. */
// Loading / empty / error states and the content swap between them.
// One anatomy for every "nothing to show" screen: icon, title, one line, action.
// crossFade() keeps the frame at its largest size and fades only the content,
// so a card never jumps between loading, empty, error and loaded.
import { icon } from './icons.js';

const el = (tag, className, ...children) => {
  const node = document.createElement(tag);
  if (className) node.className = className;
  for (const child of children.flat()) if (child != null && child !== false) node.append(child instanceof Node ? child : document.createTextNode(String(child)));
  return node;
};

const reducedNow = () => {
  if (document.documentElement.dataset.motion === 'off') return true;
  try { return matchMedia('(prefers-reduced-motion: reduce)').matches; } catch { return false; }
};

function actionNode(action) {
  if (!action) return null;
  if (action instanceof Node) return action;
  const button = el('button', `ds-btn${action.variant ? ` ds-btn--${action.variant}` : ''}`, action.icon ? icon(action.icon) : null, action.label);
  button.type = 'button';
  button.addEventListener('click', async () => {
    const result = action.onClick?.();
    if (result?.then) {
      button.setAttribute('aria-busy', 'true');
      try { await result; } finally { button.removeAttribute('aria-busy'); }
    }
  });
  return button;
}

/**
 * Empty state. action: a Node, or { label, onClick, icon, variant }.
 * Keep text to one sentence: the frame is sized for it.
 */
export function empty({ icon: name = 'inbox', title, text, action, tone } = {}) {
  const node = el('div', `ds-empty${tone ? ` ds-empty--${tone}` : ''}`,
    el('div', 'ds-empty__icon', icon(name, { className: 'ds-icon--lg' })),
    el('p', 'ds-empty__title', title),
    text ? el('p', 'ds-empty__text', text) : null);
  const button = actionNode(action);
  if (button) node.append(el('div', 'ds-empty__action', button));
  return node;
}

/** Error state: same anatomy as empty(), danger-tinted icon, retry action. */
export function errorState({ title = 'Не удалось загрузить', text = 'Проверьте соединение и попробуйте ещё раз.', onRetry, retryLabel = 'Повторить', icon: name = 'alert' } = {}) {
  const node = empty({ icon: name, title, text, tone: 'error', action: onRetry && { label: retryLabel, icon: 'refresh', onClick: onRetry } });
  node.setAttribute('role', 'alert');
  return node;
}

/**
 * Skeleton placeholder with a sheen. Lines are one body line tall each, so the
 * placeholder holds the height of the text it stands in for.
 *   skeleton({ lines: 3 })               plain text block
 *   skeleton({ lines: 2, lead: true })   a list row with an avatar / icon tile
 *   skeleton({ block: '7rem' })          a picture or chart area
 *   skeleton({ rows: 3 })                a list: same row height, lead and lines as row()
 *                                        (plain: true for a list without its own card)
 */
export function skeleton({ lines = 3, lead = false, block, height, rows, plain = false, label = 'Загрузка' } = {}) {
  if (rows) {
    const list = el('ul', `ds-list${plain ? ' ds-list--plain' : ''} ds-skeleton-list`);
    list.setAttribute('aria-busy', 'true');
    list.setAttribute('role', 'status');
    list.setAttribute('aria-label', label);
    for (let i = 0; i < rows; i++) {
      // Same .ds-row box (padding, min-height, separators) as the loaded rows.
      list.append(el('li', null, el('div', 'ds-row',
        el('span', 'ds-skeleton ds-skeleton--lead'),
        el('span', 'ds-row__body',
          el('span', 'ds-skeleton ds-skeleton--line'),
          el('span', 'ds-skeleton ds-skeleton--line ds-skeleton--line-sm')))));
    }
    list.querySelectorAll('.ds-skeleton--line:first-child').forEach((line, i) => { line.style.width = `${[72, 56, 64][i % 3]}%`; });
    list.querySelectorAll('.ds-skeleton--line-sm').forEach((line, i) => { line.style.width = `${[38, 30, 44][i % 3]}%`; });
    return list;
  }
  const group = el('div', 'ds-skeleton-group');
  group.setAttribute('aria-busy', 'true');
  group.setAttribute('role', 'status');
  group.setAttribute('aria-label', label);
  if (block) {
    const area = el('div', 'ds-skeleton ds-skeleton--block');
    area.style.height = block;
    group.append(area);
  }
  const text = el('div', 'ds-skeleton-group__lines');
  for (let i = 0; i < lines; i++) {
    const line = el('div', 'ds-skeleton ds-skeleton--line');
    if (height) line.style.height = height;
    if (i === lines - 1 && lines > 1) line.style.width = '60%';
    text.append(line);
  }
  if (lead) group.append(el('div', 'ds-skeleton-group__row', el('div', 'ds-skeleton ds-skeleton--circle'), text));
  else if (lines) group.append(text);
  return group;
}

const LAYER = 'ds-xfade__layer';

/**
 * Swaps the content of `container` with a cross-fade.
 *   render(layer) fills the new layer (append / innerHTML) or returns a Node / [Node] / string.
 * The container is locked to the largest height it has shown (min-height), so the
 * frame never shrinks or jumps between states; the overflow of a smaller state
 * is empty space inside the frame. Pass { lock: false } to let it follow the
 * content, or call releaseCrossFade() when the layout itself changes (resize).
 * Resolves when the old content is gone.
 */
export function crossFade(container, render, { duration, lock = true } = {}) {
  container.classList.add('ds-xfade');
  // First call: adopt the current children as the outgoing layer.
  let layers = [...container.children].filter((c) => c.classList.contains(LAYER));
  if (!layers.length && container.childNodes.length) {
    const first = el('div', LAYER);
    first.append(...container.childNodes);
    container.append(first);
    layers = [first];
  }
  const next = el('div', LAYER);
  const out = render?.(next);
  if (typeof out === 'string') next.innerHTML = out;
  else if (out instanceof Node || Array.isArray(out)) next.append(...[out].flat().filter(Boolean));

  const before = container.getBoundingClientRect().height;
  for (const old of layers) {
    old.getAnimations?.().forEach((a) => a.cancel());
    old.classList.add('is-leaving');
    old.setAttribute('aria-hidden', 'true');
    old.inert = true;
  }
  container.append(next);
  if (lock) {
    const locked = Number.parseFloat(container.dataset.xfadeLock || '0');
    const height = Math.max(locked, before, next.getBoundingClientRect().height);
    container.dataset.xfadeLock = String(height);
    container.style.minHeight = `${height}px`;
  }

  const removeOld = () => { for (const old of layers) old.remove(); };
  if (reducedNow() || !layers.length || typeof next.animate !== 'function') {
    removeOld();
    return Promise.resolve();
  }
  const ms = duration ?? (Number.parseFloat(getComputedStyle(container).getPropertyValue('--ds-motion-crossfade')) || 200);
  const easing = 'cubic-bezier(0.2, 0, 0, 1)';
  next.animate([{ opacity: 0 }, { opacity: 1 }], { duration: ms, easing });
  const fades = layers.map((old) => old.animate([{ opacity: 1 }, { opacity: 0 }], { duration: ms, easing, fill: 'forwards' }).finished.catch(() => {}));
  return Promise.all(fades).then(removeOld);
}

/** Drops the size lock of a crossFade() container (call on resize or layout change). */
export function releaseCrossFade(container) {
  delete container.dataset.xfadeLock;
  container.style.minHeight = '';
}
