import './style.css';

const root = document.documentElement;
root.classList.add('js');

/* ── Theme: auto (system) → light → dark. Saved in localStorage. ── */
const THEMES = ['auto', 'light', 'dark'];
const themeBtn = document.getElementById('theme');
const themeLabel = document.getElementById('theme-label');

function currentTheme() {
  const t = root.getAttribute('data-theme');
  return t === 'light' || t === 'dark' ? t : 'auto';
}
function paintThemeButton() {
  const mode = currentTheme();
  themeBtn.dataset.mode = mode;
  themeLabel.textContent = mode[0].toUpperCase() + mode.slice(1);
  themeBtn.setAttribute(
    'aria-label',
    `Colour theme: ${mode === 'auto' ? 'auto (follows system)' : mode}. Click to switch to ${THEMES[(THEMES.indexOf(mode) + 1) % 3]}.`,
  );
}
themeBtn.addEventListener('click', () => {
  const next = THEMES[(THEMES.indexOf(currentTheme()) + 1) % THEMES.length];
  if (next === 'auto') root.removeAttribute('data-theme');
  else root.setAttribute('data-theme', next);
  try { next === 'auto' ? localStorage.removeItem('sandcrab-theme') : localStorage.setItem('sandcrab-theme', next); } catch {}
  paintThemeButton();
});
paintThemeButton();

/* ── Sticky nav border once scrolled ── */
const nav = document.querySelector('.nav');
const onScroll = () => nav.classList.toggle('stuck', window.scrollY > 8);
addEventListener('scroll', onScroll, { passive: true });
onScroll();

/* ── Copy buttons ── */
document.querySelectorAll('.copy').forEach((btn) => {
  btn.addEventListener('click', async () => {
    const pre = document.querySelector(btn.dataset.copyTarget);
    if (!pre) return;
    const text = pre.innerText.replace(/\n{3,}/g, '\n\n').trim();
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      const ta = Object.assign(document.createElement('textarea'), { value: text });
      ta.style.cssText = 'position:fixed;opacity:0';
      document.body.appendChild(ta); ta.select();
      try { document.execCommand('copy'); } catch {}
      ta.remove();
    }
    const old = btn.textContent;
    btn.textContent = 'Copied'; btn.classList.add('done');
    setTimeout(() => { btn.textContent = old; btn.classList.remove('done'); }, 1600);
  });
});

/* ── Scroll reveal ── */
const revealEls = document.querySelectorAll('.section-head, .split-copy, .steps li, .mini, .wide-fig, .be, .guard, .toolset, .code, .term, .oss-points li, .notyet, .stats-block, .chooser, .table-wrap, .tools-art');
if ('IntersectionObserver' in window && !matchMedia('(prefers-reduced-motion: reduce)').matches) {
  const io = new IntersectionObserver((entries) => {
    for (const e of entries) if (e.isIntersecting) { e.target.classList.add('in'); io.unobserve(e.target); }
  }, { rootMargin: '0px 0px -8% 0px', threshold: 0.05 });
  revealEls.forEach((el) => { el.classList.add('reveal'); io.observe(el); });
}

/* ── 3D scenes (kit copied to ./kit, three.js vendored: no CDN) ──
   The kit is loaded lazily so the page paints first; each scene mounts when it nears the viewport.
   Without WebGL the illustrated poster stays in place. */
const fallbackBase = new URL('assets/crab/', document.baseURI).href;
const smallScreen = matchMedia('(max-width: 860px)').matches;
const sceneOpts = {
  jar: { params: { maxDpr: smallScreen ? 1.5 : 2 } },
  molt: { params: { maxDpr: smallScreen ? 1.5 : 2, pitch: 0.66 } },
  wire: { params: { maxDpr: smallScreen ? 1.5 : 2 } },
  burrow: { params: { maxDpr: smallScreen ? 1.5 : 2 } },
};
let kitPromise;
const loadKit = () => (kitPromise ??= import('./kit3d/src/index.js'));

async function mountStage(el) {
  if (el.dataset.mounted) return;
  el.dataset.mounted = '1';
  try {
    const { mount, createStage, SCENES } = await loadKit();
    const name = el.dataset.scene;
    let stage;
    if (name === 'jar') {
      // Hero: the crab inside a transparent glass jar (a sandbox). The scene is ~4.3 world units wide
      // and ~4.1 tall (lid lifted, ground shadow included). The kit frames by the short side only, so we
      // compute the zoom from the element's aspect ratio and fit the whole scene at every breakpoint.
      const def = SCENES.jar;
      const alt = 'A coral crab inside a transparent glass jar, a sandbox. The lid screws down to seal it, then lifts again.';
      const view = { ...def.meta.view, target: [0.15, 1.05, 0], radius: 1.6 };
      const meta = { ...def.meta, alt, view };
      stage = createStage(el, { create: def.create, meta }, { fallbackBase, params: sceneOpts.jar.params });
      if (stage.webgl) {
        const NEED_W = 4.4, NEED_H = 4.2; // world units that must stay visible (width, height)
        const fit = () => {
          const r = el.getBoundingClientRect();
          if (!r.height) return;
          const halfH = Math.max(NEED_H / 2, NEED_W / 2 / (r.width / r.height));
          stage.set({ fraction: view.radius / halfH });
        };
        new ResizeObserver(fit).observe(el);
        fit();
      }
    } else if (name === 'molt') {
      // Band framing: the kit frames by the short side, which left a wide, flat scene floating in a
      // big card. Re-centre the camera on the scene (target) and zoom so it fills the card's width
      // with an even margin, recomputing the zoom whenever the card's aspect changes.
      const def = SCENES.molt;
      const meta = { ...def.meta, view: { ...def.meta.view, target: [0.705, -0.07, 0.1] } };
      stage = createStage(el, { create: def.create, meta }, { fallbackBase, params: { fraction: 0.9, ...sceneOpts.molt.params } });
      if (stage.webgl) {
        const fit = () => {
          const r = el.getBoundingClientRect();
          // the scene is ~5.5 world units wide; fraction scales with aspect so it spans ~84% of the width
          if (r.height) stage.set({ fraction: Math.min(1.15, Math.max(0.6, 0.666 * (r.width / r.height))) });
        };
        new ResizeObserver(fit).observe(el);
        fit();
      }
    } else {
      stage = mount(el, name, { fallbackBase, ...sceneOpts[name] });
    }
    if (stage.webgl) {
      el.classList.add('live');
    } else {
      // No WebGL: keep our own poster, drop the kit's duplicate fallback image.
      stage.fallback?.remove();
      el.classList.add('no-3d');
    }
  } catch (err) {
    console.warn('3D scene failed, showing poster instead', err);
    el.classList.add('no-3d');
  }
}

const stages = [...document.querySelectorAll('.stage[data-scene]')];
const pending = stages.filter((el) => !el.hasAttribute('data-eager'));
stages.filter((el) => el.hasAttribute('data-eager')).forEach(mountStage);
if ('IntersectionObserver' in window) {
  const io = new IntersectionObserver((entries) => {
    for (const e of entries) if (e.isIntersecting) { io.unobserve(e.target); mountStage(e.target); }
  }, { rootMargin: '400px 0px' });
  pending.forEach((el) => io.observe(el));
} else {
  pending.forEach(mountStage);
}
