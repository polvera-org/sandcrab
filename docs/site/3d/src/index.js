// Sandcrab 3D kit — public entry point.
//   import { mount } from './kit/src/index.js';
//   const stage = mount(document.querySelector('#hero'), 'jar');
import { createStage } from './scene.js';
import { createCrabScene, crabMeta } from './crab.js';
import { createJar, jarMeta } from './jar.js';
import { createTidepool, tidepoolMeta } from './tidepool.js';
import { createBurrow, burrowMeta } from './burrow.js';
import { createMolt, moltMeta } from './molt.js';
import { createWire, wireMeta } from './wire.js';
import { createToolbelt, toolbeltMeta } from './toolbelt.js';

export { THREE, PALETTE, createKit, setOutlineWidth, ease, win, bump, lerp, clamp01, rng, timeline } from './kit.js';
export { createStage, hasWebGL, DEFAULT_PARAMS } from './scene.js';
export { createCrab, createCrabScene, crabMeta } from './crab.js';
export { createJar, createJarBody, jarMeta } from './jar.js';
export { createTidepool, tidepoolMeta } from './tidepool.js';
export { createBurrow, burrowMeta } from './burrow.js';
export { createMolt, moltMeta } from './molt.js';
export { createWire, wireMeta } from './wire.js';
export { createToolbelt, toolbeltMeta, TOOLS } from './toolbelt.js';

/** Scene registry: name → { create(opts), meta }. */
export const SCENES = {
  crab: { create: createCrabScene, meta: crabMeta },
  jar: { create: createJar, meta: jarMeta },
  tidepool: { create: createTidepool, meta: tidepoolMeta },
  burrow: { create: createBurrow, meta: burrowMeta },
  molt: { create: createMolt, meta: moltMeta },
  wire: { create: createWire, meta: wireMeta },
  toolbelt: { create: createToolbelt, meta: toolbeltMeta },
};

/**
 * Mount a named scene into an element (the element sets the size).
 * @param {HTMLElement} el
 * @param {keyof SCENES} name  'crab'|'jar'|'tidepool'|'burrow'|'molt'|'wire'|'toolbelt'
 * @param {object} [opts]  { params, fallbackBase, theme, t0, ...scene options (e.g. pose) }
 * @returns stage controller: { set, seek, play, pause, render, info, dispose, time, params, meta, webgl }
 */
export function mount(el, name, opts = {}) {
  if (!el) throw new Error('sandcrab-3d: mount() needs an element');
  const def = SCENES[name];
  if (!def) throw new Error(`sandcrab-3d: unknown scene "${name}" (have: ${Object.keys(SCENES).join(', ')})`);
  const { params, fallbackBase, theme, t0, ...sceneOpts } = opts;
  return createStage(el, def, { params, fallbackBase, theme, t0, sceneOpts });
}
