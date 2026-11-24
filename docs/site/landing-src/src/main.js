import Lenis from 'lenis';
import 'lenis/dist/lenis.css';
import './style.css';

const root = document.documentElement;
root.classList.add('js');

/* ── Theme: light by default; dark only when chosen here. Saved in localStorage. ── */
const themeBtn = document.getElementById('theme');
const themeLabel = document.getElementById('theme-label');
const isDarkTheme = () => root.getAttribute('data-theme') === 'dark';
function paintThemeButton() {
  const dark = isDarkTheme();
  themeBtn.dataset.mode = dark ? 'dark' : 'light';
  themeBtn.setAttribute('aria-pressed', String(dark));
  themeBtn.setAttribute('aria-label', dark ? 'Dark theme on. Switch to light theme' : 'Light theme on. Switch to dark theme');
  themeLabel.textContent = dark ? 'Dark theme' : 'Light theme';
}
function setTheme(dark) {
  if (dark) root.setAttribute('data-theme', 'dark'); else root.removeAttribute('data-theme');
  document.querySelector('meta[name="color-scheme"]')?.setAttribute('content', dark ? 'dark' : 'light');
  document.querySelector('meta[name="theme-color"]')?.setAttribute('content', dark ? '#111A20' : '#FAFAF7');
  try { dark ? localStorage.setItem('sandcrab-theme', 'dark') : localStorage.removeItem('sandcrab-theme'); } catch {}
  paintThemeButton();
}
themeBtn.addEventListener('click', () => setTheme(!isDarkTheme()));
paintThemeButton();

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

/* ── Header: solid backdrop always; hairline only once the page is scrolled ── */
const bar = document.querySelector('.bar');
const paintBar = () => bar?.classList.toggle('scrolled', window.scrollY > 4);
addEventListener('scroll', paintBar, { passive: true });
paintBar();

/* ── Inertial smooth scroll (Lenis) so the scroll-scrubbed film plays out instead of skipping.
   Wheel/trackpad only: touch keeps native scroll; keyboard scrolling stays native and Lenis
   follows it. Off entirely under prefers-reduced-motion. The film drives lenis.raf() from its
   own rAF tick (same frame as the render); until then, or without WebGL, we drive it here. */
let lenis = null;
let ownRaf = 0;
const lenisLoop = (t) => { lenis.raf(t); ownRaf = requestAnimationFrame(lenisLoop); };
const driveLenis = () => { if (lenis && !ownRaf) ownRaf = requestAnimationFrame(lenisLoop); };
const releaseLenis = () => { cancelAnimationFrame(ownRaf); ownRaf = 0; };
if (!matchMedia('(prefers-reduced-motion: reduce)').matches) {
  lenis = new Lenis({ lerp: 0.08, smoothWheel: true, syncTouch: false, wheelMultiplier: 0.9, autoRaf: false });
  driveLenis();
  // In-page links go through Lenis, stopping below the sticky header.
  document.addEventListener('click', (e) => {
    const a = e.target.closest?.('a[href^="#"]');
    if (!a || a.classList.contains('skip') || e.defaultPrevented || e.metaKey || e.ctrlKey) return;
    const hash = a.getAttribute('href');
    const target = hash === '#top' ? 0 : document.querySelector(hash);
    if (target == null) return;
    e.preventDefault();
    const offset = -((bar?.getBoundingClientRect().height ?? 72) + 8);
    lenis.scrollTo(target, { offset: target === 0 ? 0 : offset, duration: 1.6 });
    history.pushState(null, '', hash);
  });
}

/* ── The narrative film: one fixed canvas, one crab for the whole page ──
   Loaded after first paint (three.js is vendored: no CDN). Without WebGL, the page shows
   a poster in each section's crab column instead (html.no-3d). */
const film = document.getElementById('crab-film');
function no3d(err) {
  if (err) console.warn('3D film unavailable, showing posters instead', err);
  root.classList.add('no-3d');
  film?.replaceChildren();
  driveLenis();
}
function hasWebGL2() {
  try { return !!(window.WebGL2RenderingContext && document.createElement('canvas').getContext('webgl2')); } catch { return false; }
}
if (!film || !hasWebGL2()) no3d();
else {
  const go = () => import('./narrative.js')
    .then(({ startFilm }) => { releaseLenis(); if (!startFilm(film, { onFail: no3d, lenis })) no3d(); })
    .catch(no3d);
  if (document.readyState === 'complete') go(); else addEventListener('load', go, { once: true });
}
