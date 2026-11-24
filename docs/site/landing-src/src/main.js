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
  themeLabel.textContent = `Theme: ${mode}`;
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

/* ── The narrative film: one fixed canvas, one crab for the whole page ──
   Loaded after first paint (three.js is vendored: no CDN). Without WebGL, the page shows
   a poster in each section's crab column instead (html.no-3d). */
const film = document.getElementById('crab-film');
function no3d(err) {
  if (err) console.warn('3D film unavailable, showing posters instead', err);
  root.classList.add('no-3d');
  film?.replaceChildren();
}
function hasWebGL2() {
  try { return !!(window.WebGL2RenderingContext && document.createElement('canvas').getContext('webgl2')); } catch { return false; }
}
if (!film || !hasWebGL2()) no3d();
else {
  const go = () => import('./narrative.js')
    .then(({ startFilm }) => { if (!startFilm(film, { onFail: no3d })) no3d(); })
    .catch(no3d);
  if (document.readyState === 'complete') go(); else addEventListener('load', go, { once: true });
}
