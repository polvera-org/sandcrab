// Presentation scenes (docs/site/presentation.html), built from the kit's crab,
// jar, tide pool, burrow and toolbelt. Each entry is a kit scene definition
// ({ create(opts), meta }), so any of them also works with createStage():
//   import { createStage } from '../index.js';
//   import { PRES_SCENES } from './index.js';
//   createStage(el, PRES_SCENES.bars, { sceneOpts: { data: window.SANDCRAB_DATA } });
// Scenes that show numbers take opts.data (presentation/data.js); `screen` takes
// opts.screenshot. Scenes also return `labels` (see common.js) for an HTML overlay.
import { createShore, shoreMeta } from './shore.js';
import { createSharedbox, sharedboxMeta } from './sharedbox.js';
import { createLifeline, lifelineMeta } from './lifeline.js';
import { createFrames, framesMeta } from './frames.js';
import { createTools, toolsMeta } from './tools.js';
import { createMounts, mountsMeta } from './mounts.js';
import { createIsolation, isolationMeta } from './isolation.js';
import { createSafety, safetyMeta } from './safety.js';
import { createBars, barsMeta } from './bars.js';
import { createScreen, screenMeta } from './screen.js';
import { createRoadmap, roadmapMeta } from './roadmap.js';

export const PRES_SCENES = {
  shore: { create: createShore, meta: shoreMeta },
  sharedbox: { create: createSharedbox, meta: sharedboxMeta },
  lifeline: { create: createLifeline, meta: lifelineMeta },
  frames: { create: createFrames, meta: framesMeta },
  tools: { create: createTools, meta: toolsMeta },
  mounts: { create: createMounts, meta: mountsMeta },
  isolation: { create: createIsolation, meta: isolationMeta },
  safety: { create: createSafety, meta: safetyMeta },
  bars: { create: createBars, meta: barsMeta },
  screen: { create: createScreen, meta: screenMeta },
  roadmap: { create: createRoadmap, meta: roadmapMeta },
};
