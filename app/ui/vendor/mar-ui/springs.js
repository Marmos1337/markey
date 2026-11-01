/* mar-ui 1.0.0 7af02f8 — vendored by mar-ui/tools/vendor.mjs; do not edit here, update the kit and re-vendor. */
// Spring physics shared by CSS and JS. The CSS side is the --ds-ease-spring-soft /
// --ds-ease-spring-bouncy linear() easings in tokens.css over --ds-motion-spring(-bouncy);
// tools/build-springs.mjs writes them from the numbers below and
// tools/springs.test.mjs fails when the two drift apart.
// Values: MarGram packages/ui/src/motion/springs.ts (mass 1).

export const springs = Object.freeze({
  /** Small overshoot (~4 %): tab indicator, segmented highlight, sheets, menus. */
  soft: Object.freeze({ stiffness: 224, damping: 21.6, duration: 560, stops: 25 }),
  /** Visible overshoot (~13 %): a selected chip popping, a send button. */
  bouncy: Object.freeze({ stiffness: 240, damping: 17, duration: 680, stops: 29 }),
});

/** Used where linear() is unsupported (older WebViews): a cubic with a slight overshoot. */
export const SPRING_FALLBACK = 'cubic-bezier(0.3, 1.25, 0.55, 1)';

/** Position of a unit spring (0 -> 1, mass 1) after `t` seconds, finely integrated. */
export function springAt(stiffness, damping, t) {
  let position = 0;
  let velocity = 0;
  const dt = 1 / 4000;
  const steps = Math.round(t / dt);
  for (let i = 0; i < steps; i++) {
    velocity += (-stiffness * (position - 1) - damping * velocity) * dt;
    position += velocity * dt;
  }
  return position;
}

/** Evenly spaced samples of the named spring over its duration; ends exactly on 1. */
export function springStops(name) {
  const spring = springs[name];
  if (!spring) throw new Error(`unknown spring "${name}"`);
  const { stiffness, damping, duration, stops } = spring;
  return Array.from({ length: stops }, (_, i) => {
    if (i === stops - 1) return 1;
    const value = springAt(stiffness, damping, (i / (stops - 1)) * (duration / 1000));
    return Math.round(value * 1000) / 1000;
  });
}

/** CSS linear() easing of the named spring. */
export const springLinear = (name) => `linear(${springStops(name).join(', ')})`;

let linearSupported;
const supportsLinear = () => {
  if (linearSupported === undefined) {
    try { linearSupported = !!globalThis.CSS?.supports?.('transition-timing-function', 'linear(0, 1)'); } catch { linearSupported = false; }
  }
  return linearSupported;
};

/**
 * Web Animations options for the named spring:
 *   el.animate(frames, springTiming('soft'))
 * Reduced motion returns a 1 ms timing (end events still fire).
 */
export function springTiming(name = 'soft', { reducedMotion = false } = {}) {
  if (reducedMotion) return { duration: 1, easing: 'linear' };
  const spring = springs[name];
  if (!spring) throw new Error(`unknown spring "${name}"`);
  return { duration: spring.duration, easing: supportsLinear() ? springLinear(name) : SPRING_FALLBACK };
}
