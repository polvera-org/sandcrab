# Sandcrab 3D kit

Seven small three.js scenes of the Sandcrab crab, one product idea each. They are
plain ES modules with no build step and no CDN: three.js r186 is vendored under
`vendor/`, and every model is built in code. Open `index.html` to see the gallery
(`?tune` adds a tuning panel).

## Quick start

Paths below assume the kit is mounted at `/kit` (read-only) and your page sits at
the web root. Change `./kit/` if it lives somewhere else.

```html
<div id="hero" style="aspect-ratio: 4 / 3"></div>
<script type="module">
  import { mount } from './kit/src/index.js';
  mount(document.querySelector('#hero'), 'jar');
</script>
```

The element decides the size. The canvas fills it and follows resizes. Give it a
height (aspect-ratio or a fixed height), or nothing will show.

## Scenes

| Name | Idea (one sentence) | Loop |
| --- | --- | --- |
| `crab` | The mascot. Options: `pose: 'idle' \| 'wave' \| 'work'` (default `wave`). | 12 s |
| `jar` | Hardware isolation: every crab gets its own sealed jar (microVM). | 12 s |
| `tidepool` | Shared kernel (Docker): crabs share one pool, behind a barrier. Option: `count` (2–7). | 12 s |
| `burrow` | Persistent workspace: sandboxes come and go, the crates (files) stay. | 10 s |
| `molt` | Ephemeral sandbox: the shell is discarded, the output (teal cube) survives. | 12 s |
| `wire` | One guest protocol: one cable between host and jar carries every request and result. | 12 s |
| `toolbelt` | Native tools: the claw holds one tool card at a time (bash, file_read, file_edit, grep, glob, list_dir). | 18 s |

## API

```js
import { mount, SCENES, createCrab, createStage, PALETTE, THREE } from './kit/src/index.js';

const stage = mount(el, 'burrow', {
  params: { speed: 1, outline: 1.6, maxDpr: 2 },  // optional tunables, see below
  theme: 'auto',                                  // 'auto' | 'light' | 'dark'
  fallbackBase: '/assets/crab/',                  // where the fallback .webp files live
  t0: 0,                                          // start time, seconds
  // any other key goes to the scene, for example pose: 'work' for 'crab'
});

stage.pause(); stage.play();
stage.seek(4.2);              // jump to a time (seconds), then render
stage.set({ speed: 0.75 });   // change tunables live
stage.dispose();              // stop, free GPU memory, remove the canvas
stage.webgl;                  // false when the fallback image is shown
stage.meta;                   // { title, loop, phases, still, view, fallback, alt }
```

Tunables (`params` / `stage.set`):

| Key | Default | Meaning |
| --- | --- | --- |
| `speed` | `1` | Time multiplier. |
| `yaw`, `pitch` | per scene | Camera angles in radians. |
| `fraction` | per scene | How much of the short side the subject fills (0.3–0.95). |
| `sway` | per scene | Camera yaw sway in radians; one sway per loop. |
| `outline` | `1.6` | Ink outline width in CSS px (`0` turns it off). |
| `shadow` | theme | Ground shadow opacity. |
| `maxDpr` | `2` | Device pixel ratio cap. |
| `paused` | `false` | Freeze time. |

### Composing your own scene

Each module exports a factory that returns `{ object3d, update(t, dt), dispose(), meta }`.
Use them on their own, or pass a `{ create, meta }` definition to `createStage`:

```js
import { createStage, createCrab, createKit, THREE } from './kit/src/index.js';

const def = {
  meta: { title: 'Two crabs', fallback: 'reef', loop: 12, still: 1,
          view: { target: [0, 0.4, 0], radius: 2.6, yaw: 0.5, pitch: 0.5, fraction: 0.75 } },
  create() {
    const k = createKit();
    const g = new THREE.Group();
    g.add(k.slab(5, 2.6));                        // sand tile, top at y = 0
    const a = createCrab({ pose: 'wave' }); a.object3d.position.x = -1.3;
    const b = createCrab({ pose: 'work', phase: 0.5 }); b.object3d.position.x = 1.3;
    g.add(a.object3d, b.object3d);
    return { object3d: g, update: (t, dt) => { a.update(t, dt); b.update(t, dt); },
             dispose: () => { a.dispose(); b.dispose(); k.dispose(); } };
  },
};
createStage(el, def);
```

`createCrab(opts)`: `scale`, `pose` (`idle | wave | work | hold`), `shell`
(`coral | husk | grey`), `detail` (`high | low`), `phase` (time offset). The crab
faces +Z and walks sideways along X. Extra methods: `setPose`, `setWalk(0..1)`,
`setOpacity`, `setTint(hex, k)`.

## Behaviour built in

- **Offline**: no network requests except the fallback image.
- **Reduced motion**: if `prefers-reduced-motion: reduce` is set, the scene renders
  one still frame (`meta.still`) and does not animate.
- **Offscreen**: animation pauses when the element is out of view or the tab is
  hidden.
- **Theme**: the canvas is transparent, so your page background shows through. The
  shadow strength follows `prefers-color-scheme` and `<html data-theme="light|dark">`.
- **No WebGL** (or the context is lost): shows `<fallback>.webp` from
  `fallbackBase`. By default that is `../../assets/crab/` next to `src/`, which is
  `docs/site/assets/crab/` in the repo. If only the kit is mounted, copy those
  `.webp` files into your site and pass `fallbackBase`.
- **Seamless loops**: every motion is a function of time, with periods that divide
  the loop length.

## Performance

- Each scene has about 1.8k to 5k triangles of fill. The ink outline is an
  inverted hull, so it roughly doubles the count. A frame costs 80 to 400 draw
  calls; `tidepool` is the heaviest.
- Each `mount` creates its own WebGL context, and browsers allow about 16. Use
  one to three scenes per page and `dispose()` the ones you remove.
- `maxDpr: 1.5` is a good setting for large hero canvases on phones.
- Shared three.js is about 765 KB minified (about 190 KB gzipped). Serve it with
  gzip or brotli if you can.

## Still renders

`render.html` renders one scene frame to a transparent PNG: no sand, no ground shadow, fixed
time. Shots: `mascot`, `jar`, `crates`. Query: `?shot=jar&w=1200&h=1200&outline=3.5`, plus
optional `t`, `yaw`, `pitch`, `fraction`, `dpr`, and `download=1` to save the PNG. Serve this
folder (`python3 -m http.server`) and either open the page with `download=1`, or run headless
Chrome with `--headless=new --use-angle=swiftshader --enable-unsafe-swiftshader
--force-device-scale-factor=2 --virtual-time-budget=8000 --dump-dom` and decode the data URL
from `<pre id="out">`. The README hero (`docs/assets/sandcrab-hero.png`) is the `jar` shot.

## Palette

`PALETTE`: sand `#F5ECD9` / `#E8D5B0`, teal `#1F7A8C` / `#0F4C5C`, ink `#0B2F38`,
coral `#FF6B4A` (used only for the crab). Art direction: `../symbology.html`.
