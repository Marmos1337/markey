/* mar-ui 1.0.0 7af02f8 — vendored by mar-ui/tools/vendor.mjs; do not edit here, update the kit and re-vendor. */
// Animated icons. Every kit icon has one short motion (from MarMic's icon-motion
// table) that plays ONCE: on hover with a fine pointer, on press with a finger,
// on keyboard focus, and when its control becomes selected/pressed/current.
// Nothing runs while idle: the CSS animation exists only while [data-icon-play]
// is set, and the attribute is removed when the play ends.
// Morph pairs (sun <-> moon, mic <-> mic-off ...) are two stacked glyphs that
// cross-fade with a scale / turn / clip; reduced motion swaps them instantly.
import { iconSvg } from './icons.js';

/** The ten motions; keyframes live in icon-motion.css. */
export const MOTIONS = Object.freeze(['nudge', 'pop', 'swing', 'turn', 'spin', 'drop', 'lift', 'slide', 'shake', 'blink']);

/** Kit icon name -> motion. tools/icon-motion.test.mjs requires an entry for every icon. */
export const ICON_MOTION = Object.freeze({
  // pop: adding, confirming, people, rewards
  plus: 'pop', check: 'pop', checkCircle: 'pop', circle: 'pop', heart: 'pop', star: 'pop', spark: 'pop',
  flag: 'pop', pin: 'pop', spot: 'pop', invite: 'pop', user: 'pop', people: 'pop', chat: 'pop',
  coins: 'pop', bag: 'pop', image: 'pop', copy: 'pop', grid: 'pop', layers: 'pop', max: 'pop',
  // swing from the top: things that hang or ring
  bell: 'swing', bellOff: 'swing', hook: 'swing', diary: 'swing', paperclip: 'swing', tool: 'swing',
  // a quarter turn: dials and knobs
  settings: 'turn', sun: 'turn', sunMoon: 'turn', cast: 'turn', meter: 'turn', locate: 'turn',
  clock: 'turn', globe: 'turn', moon: 'turn',
  // a full turn: reload and repeat
  refresh: 'spin', repeat: 'spin', history: 'spin',
  // down: saving, digging, opening a list
  download: 'drop', depth: 'drop', down: 'drop', drop: 'drop', inbox: 'drop', archive: 'drop', water: 'drop',
  // up: sending, uploading, taking off
  upload: 'lift', up: 'lift', send: 'lift', telegram: 'lift', plane: 'lift', edge: 'lift', lightning: 'lift',
  // sideways: navigating (back / left / undo slide the other way, see icon-motion.css)
  arrow: 'slide', right: 'slide', chevron: 'slide', back: 'slide', left: 'slide', undo: 'slide',
  logout: 'slide', external: 'slide', share: 'slide', route: 'slide', wind: 'slide', link: 'slide',
  sort: 'slide', sliders: 'slide', filter: 'slide', list: 'slide', tasks: 'slide', menu: 'slide', more: 'slide',
  // shake: destructive and warnings
  trash: 'shake', x: 'shake', close: 'shake', warning: 'shake', alert: 'shake', obstacle: 'shake',
  lock: 'shake', unlock: 'shake', wifiOff: 'shake', minus: 'shake', stop: 'shake',
  // blink: looking and capturing
  eye: 'blink', eyeOff: 'blink', camera: 'blink', search: 'blink',
  // nudge: a small scale + tilt for the rest
  access: 'nudge', area: 'nudge', book: 'nudge', building: 'nudge', calendar: 'nudge', chart: 'nudge',
  edit: 'nudge', feed: 'nudge', file: 'nudge', fish: 'nudge', folder: 'nudge', home: 'nudge', info: 'nudge',
  map: 'nudge', mic: 'nudge', micOff: 'nudge', motion: 'nudge', pause: 'nudge', phone: 'nudge', play: 'nudge',
  shield: 'nudge', sound: 'nudge', wallet: 'nudge',
});

/**
 * State pairs. a = off / initial, b = on. kind picks the transition:
 *   turn   — a turns out, b turns in (theme)
 *   scale  — a shrinks out, b grows in (play / pause)
 *   reveal — b wipes in left to right over a (to-do done)
 *   strike — b (the struck glyph) is revealed from the top-left corner, like a
 *            strike being drawn; a fades under it (eye, mic, bell)
 */
export const MORPH_PAIRS = Object.freeze({
  theme: Object.freeze({ a: 'sun', b: 'moon', kind: 'turn' }),
  check: Object.freeze({ a: 'circle', b: 'checkCircle', kind: 'reveal' }),
  play: Object.freeze({ a: 'play', b: 'pause', kind: 'scale' }),
  eye: Object.freeze({ a: 'eye', b: 'eyeOff', kind: 'strike' }),
  mic: Object.freeze({ a: 'mic', b: 'micOff', kind: 'strike' }),
  bell: Object.freeze({ a: 'bell', b: 'bellOff', kind: 'strike' }),
});

export const motionFor = (name) => ICON_MOTION[name] ?? 'nudge';

/** Controls whose icons play on hover / press / focus. Mark anything else with data-icon-trigger. */
const TRIGGER = 'button, a[href], summary, label, [role="button"], [role="tab"], [role="menuitem"], [role="switch"], [role="link"], .ds-row, .ds-chip, [data-icon-trigger]';
const STATE_ATTRS = ['aria-selected', 'aria-pressed', 'aria-current', 'aria-checked'];
const isOn = (value) => value != null && value !== 'false';
const doc = () => (typeof document === 'undefined' ? null : document);

export function reducedMotion() {
  const d = doc();
  if (!d) return true;
  if (d.documentElement.dataset.motion === 'off') return true;
  try { return matchMedia('(prefers-reduced-motion: reduce)').matches; } catch { return false; }
}

const timers = new WeakMap();

/**
 * Plays one icon's motion once. An svg may override its motion with data-icon-motion="pop".
 * Returns false when skipped (reduced motion, already playing, not a kit icon).
 */
export function playIcon(svg) {
  if (!svg?.dataset || reducedMotion()) return false;
  if (svg.dataset.iconPlay) return false; // let the current play finish: no restart jitter
  const name = svg.dataset.icon;
  if (!name) return false;
  svg.dataset.iconPlay = svg.dataset.iconMotion || motionFor(name);
  const done = () => {
    clearTimeout(timers.get(svg));
    svg.removeEventListener('animationend', done);
    svg.removeEventListener('animationcancel', done);
    delete svg.dataset.iconPlay;
  };
  svg.addEventListener('animationend', done);
  svg.addEventListener('animationcancel', done);
  // Safety net when no animation event arrives (hidden tab, display:none).
  timers.set(svg, setTimeout(done, 900));
  return true;
}

/** Icons owned by this control (not by a nested control), skipping hidden morph glyphs. */
function iconsOf(trigger) {
  return [...trigger.querySelectorAll('.ds-icon[data-icon]')].filter((svg) => {
    if (svg.closest(TRIGGER) !== trigger) return false;
    const morph = svg.closest('.ds-morph');
    if (!morph) return true;
    return svg.classList.contains(morph.dataset.state === 'b' ? 'ds-morph__b' : 'ds-morph__a');
  });
}

function triggerOf(target, root) {
  const el = target instanceof Element ? target.closest(TRIGGER) : null;
  if (!el || !(root === document || root.contains(el))) return null;
  if (el.matches(':disabled, [aria-disabled="true"]')) return null;
  return el;
}

const play = (trigger) => { for (const svg of iconsOf(trigger)) playIcon(svg); };

/**
 * Wires the plays for everything under root (default: the whole document) with
 * four delegated listeners and one attribute observer. Returns a dispose().
 */
export function initIconMotion({ root } = {}) {
  const d = doc();
  if (!d) return () => {};
  const scope = root ?? d;
  const finePointer = () => { try { return matchMedia('(hover: hover) and (pointer: fine)').matches; } catch { return true; } };

  const onOver = (e) => {
    if (e.pointerType !== 'mouse' || !finePointer()) return;
    const trigger = triggerOf(e.target, scope);
    if (!trigger || (e.relatedTarget instanceof Node && trigger.contains(e.relatedTarget))) return;
    play(trigger);
  };
  const onDown = (e) => {
    if (e.pointerType === 'mouse') return; // mouse already played on hover
    const trigger = triggerOf(e.target, scope);
    if (trigger) play(trigger);
  };
  const onFocus = (e) => {
    const trigger = triggerOf(e.target, scope);
    if (trigger && trigger === e.target && trigger.matches(':focus-visible')) play(trigger);
  };
  scope.addEventListener('pointerover', onOver, { passive: true });
  scope.addEventListener('pointerdown', onDown, { passive: true });
  scope.addEventListener('focusin', onFocus);

  // Selected / pressed / current turning on plays the control's icon once. A morph
  // inside the control is its own feedback, so morph glyphs are left alone here.
  let observer = null;
  if (typeof MutationObserver !== 'undefined') {
    observer = new MutationObserver((records) => {
      for (const r of records) {
        const el = r.target;
        if (!(el instanceof Element) || !isOn(el.getAttribute(r.attributeName)) || isOn(r.oldValue)) continue;
        if (el.matches(':disabled')) continue;
        for (const svg of iconsOf(el)) if (!svg.closest('.ds-morph')) playIcon(svg);
      }
    });
    const target = scope === d ? d.documentElement : scope;
    observer.observe(target, { subtree: true, attributes: true, attributeOldValue: true, attributeFilter: STATE_ATTRS });
  }
  return () => {
    scope.removeEventListener('pointerover', onOver);
    scope.removeEventListener('pointerdown', onDown);
    scope.removeEventListener('focusin', onFocus);
    observer?.disconnect();
  };
}

/* ---------- Morph ---------- */

function pairOf(pair) {
  const spec = typeof pair === 'string' ? MORPH_PAIRS[pair] : pair;
  if (!spec?.a || !spec?.b) throw new Error(`unknown morph pair "${pair}"`);
  return spec;
}

/**
 * Markup for a morphing icon (string-template apps). on=false shows a, on=true shows b.
 * The wrapper is aria-hidden: put the state in the control (aria-pressed, aria-label).
 */
export function morphIconSvg(pair, { on = false, className = '' } = {}) {
  const spec = pairOf(pair);
  const name = typeof pair === 'string' ? pair : `${spec.a}-${spec.b}`;
  const kind = spec.kind ?? 'scale';
  return `<span class="ds-morph ds-morph--${kind}${className ? ` ${className}` : ''}" data-morph="${name}" data-state="${on ? 'b' : 'a'}" aria-hidden="true">`
    + iconSvg(spec.a, { className: 'ds-morph__a' }) + iconSvg(spec.b, { className: 'ds-morph__b' }) + '</span>';
}

/** DOM version of morphIconSvg(). */
export function morphIcon(pair, options) {
  const tpl = document.createElement('template');
  tpl.innerHTML = morphIconSvg(pair, options);
  return tpl.content.firstElementChild;
}

/** Switches a morph (the element or any ancestor of exactly one) to a (false) or b (true). */
export function setMorph(el, on) {
  const morph = el?.classList?.contains('ds-morph') ? el : el?.querySelector?.('.ds-morph');
  if (!morph) return;
  const next = on ? 'b' : 'a';
  if (morph.dataset.state === next) return;
  morph.dataset.state = next;
}
