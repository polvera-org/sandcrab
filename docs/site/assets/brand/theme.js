/* Sandcrab brand: theme + header behaviour for the static pages in docs/site/.
   Load synchronously in <head>, after the color-scheme and theme-color metas:
     <script src="assets/brand/theme.js"></script>
   Light is the default whatever the system theme; dark applies only when chosen with the
   toggle (saved as localStorage "sandcrab-theme", shared with the landing page). */
(function () {
  var root = document.documentElement;
  var KEY = 'sandcrab-theme';
  function isDark() { return root.getAttribute('data-theme') === 'dark'; }
  function metas(dark) {
    var cs = document.querySelector('meta[name="color-scheme"]');
    var tc = document.querySelector('meta[name="theme-color"]');
    if (cs) cs.setAttribute('content', dark ? 'dark' : 'light');
    if (tc) tc.setAttribute('content', dark ? '#111A20' : '#FAFAF7');
  }
  // before first paint: no flash
  try { if (localStorage.getItem(KEY) === 'dark') root.setAttribute('data-theme', 'dark'); } catch (e) { /* storage blocked */ }
  metas(isDark());

  function paint(btn) {
    var dark = isDark();
    btn.dataset.mode = dark ? 'dark' : 'light';
    btn.setAttribute('aria-pressed', String(dark));
    btn.setAttribute('aria-label', dark ? 'Dark theme on. Switch to light theme' : 'Light theme on. Switch to dark theme');
  }
  function setTheme(dark) {
    if (dark) root.setAttribute('data-theme', 'dark'); else root.removeAttribute('data-theme');
    metas(dark);
    try { if (dark) localStorage.setItem(KEY, 'dark'); else localStorage.removeItem(KEY); } catch (e) { /* ignore */ }
    var btns = document.querySelectorAll('[data-theme-toggle]');
    for (var i = 0; i < btns.length; i++) paint(btns[i]);
  }
  window.sandcrabTheme = { isDark: isDark, set: setTheme };

  function ready() {
    var btns = document.querySelectorAll('[data-theme-toggle]');
    for (var i = 0; i < btns.length; i++) {
      paint(btns[i]);
      btns[i].addEventListener('click', function () { setTheme(!isDark()); });
    }
    // header hairline appears once the page is scrolled
    var bar = document.querySelector('.site-bar');
    if (bar) {
      var onScroll = function () { bar.classList.toggle('scrolled', window.scrollY > 4); };
      window.addEventListener('scroll', onScroll, { passive: true });
      onScroll();
    }
  }
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', ready); else ready();
})();
