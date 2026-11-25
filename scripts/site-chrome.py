#!/usr/bin/env python3
"""Sync the shared header and footer into the static pages in docs/site/.

Every page carries marker comments; this script rewrites what is between them, so the site
chrome is defined once (here) and styled once (docs/site/assets/brand/brand.css):

    <!-- SITE-HEADER:BEGIN -->...<!-- SITE-HEADER:END -->
    <!-- SITE-FOOTER:BEGIN -->...<!-- SITE-FOOTER:END -->

The landing page (docs/site/landing-src) is built by Vite and has its own copy of the same
header; keep the two visually in step.

Usage (from the repo root):
    python3 scripts/site-chrome.py          # rewrite in place
    python3 scripts/site-chrome.py --check  # exit 1 if any page is out of date
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

SITE = Path(__file__).resolve().parent.parent / "docs/site"
REPO = "https://github.com/polvera-org/sandcrab"

# (key, label, href relative to docs/site/)
NAV = [
    ("home", "Home", "landing/index.html"),
    ("walkthrough", "Walkthrough", "walkthrough.html"),
    ("bench", "Benchmarks", "bench-report.html"),
    ("symbology", "Symbology", "symbology.html"),
    ("presentation", "Presentation", "presentation.html"),
    ("docs", "Docs", f"{REPO}/tree/main/docs"),
]
# page file → the nav item it marks as current (None: no item)
PAGES = {
    "index.html": None,
    "walkthrough.html": "walkthrough",
    "bench-report.html": "bench",
    "symbology.html": "symbology",
    "presentation.html": "presentation",
}

LOGO = (
    '<svg class="site-logo" viewBox="-21 -17 42 31" aria-hidden="true" focusable="false" fill="none" '
    'stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round">'
    '<path d="M-9 4l-5 5M-5 6l-3 6M9 4l5 5M5 6l3 6"/><path d="M-9 -1l-5 -5M9 -1l5 -5"/>'
    '<circle cx="-16" cy="-8" r="4.2" fill="currentColor"/><circle cx="16" cy="-8" r="4.2" fill="currentColor"/>'
    '<path d="M-4 -7l-1 -4M4 -7l1 -4"/><path d="M-12 3a12 10 0 0 1 24 0q0 4 -12 4t-12 -4z" fill="currentColor"/>'
    '<g stroke="none"><circle cx="-5" cy="-12" r="2.8" fill="currentColor"/><circle cx="5" cy="-12" r="2.8" fill="currentColor"/>'
    '<circle class="eye" cx="-5" cy="-12" r="1.15"/><circle class="eye" cx="5" cy="-12" r="1.15"/></g></svg>'
)
GITHUB = (
    f'<a class="icon-btn" href="{REPO}" target="_blank" rel="noopener" aria-label="Sandcrab on GitHub (opens in a new tab)">'
    '<svg viewBox="0 0 24 24" aria-hidden="true" focusable="false"><path fill="currentColor" d="M12 .5a11.5 11.5 0 0 0-3.64 22.41c.58.1.79-.25.79-.56v-2c-3.2.7-3.88-1.37-3.88-1.37-.52-1.33-1.28-1.69-1.28-1.69-1.05-.71.08-.7.08-.7 1.16.08 1.77 1.19 1.77 1.19 1.03 1.77 2.7 1.26 3.36.96.1-.75.4-1.26.73-1.55-2.55-.29-5.24-1.28-5.24-5.69 0-1.26.45-2.28 1.18-3.09-.12-.29-.51-1.46.11-3.04 0 0 .97-.31 3.17 1.18a11 11 0 0 1 5.76 0c2.2-1.49 3.17-1.18 3.17-1.18.63 1.58.23 2.75.11 3.04.74.81 1.18 1.83 1.18 3.09 0 4.42-2.7 5.4-5.27 5.68.41.36.78 1.06.78 2.14v3.17c0 .31.21.67.8.56A11.5 11.5 0 0 0 12 .5Z"/></svg></a>'
)
THEME_BTN = (
    '<button class="icon-btn" type="button" data-theme-toggle aria-pressed="false" aria-label="Light theme on. Switch to dark theme">'
    '<svg class="ico ico-light" viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="4"/><path d="M12 2.5v2.5M12 19v2.5M2.5 12H5M19 12h2.5M5.3 5.3l1.8 1.8M16.9 16.9l1.8 1.8M5.3 18.7l1.8-1.8M16.9 7.1l1.8-1.8"/></svg>'
    '<svg class="ico ico-dark" viewBox="0 0 24 24" aria-hidden="true"><path d="M20 14.5A8.5 8.5 0 0 1 9.5 4a8.5 8.5 0 1 0 10.5 10.5z"/></svg>'
    '</button>'
)


def link(key: str, label: str, href: str, current: str | None) -> str:
    ext = href.startswith("http")
    attrs = ' target="_blank" rel="noopener"' if ext else ""
    cur = ' aria-current="page"' if key == current else ""
    sr = '<span class="sr-only"> (opens in a new tab)</span>' if ext else ""
    return f'<a href="{href}"{attrs}{cur}>{label}{sr}</a>'


def header(current: str | None) -> str:
    nav = "".join(link(k, l, h, current) for k, l, h in NAV)
    return (
        '<a class="sr-only" href="#main" onfocus="this.classList.remove(\'sr-only\')" onblur="this.classList.add(\'sr-only\')">Skip to content</a>\n'
        '<header class="site-bar">\n'
        '  <div class="site-bar-in">\n'
        f'    <a class="site-brand" href="landing/index.html" aria-label="Sandcrab home">{LOGO}<span>Sandcrab</span></a>\n'
        f'    <nav class="site-nav" aria-label="Site">{nav}</nav>\n'
        f'    <div class="site-actions">{GITHUB}{THEME_BTN}<a class="pill pill-sm" href="landing/index.html#start">Quickstart</a></div>\n'
        '  </div>\n'
        '</header>\n'
    )


def footer(note: str) -> str:
    nav = "".join(link(k, l, h, None) for k, l, h in NAV) + link("gh", "GitHub", REPO, None)
    return (
        '<footer class="site-foot">\n'
        '  <div class="site-foot-in">\n'
        f'    {LOGO}\n'
        f'    <nav aria-label="Footer">{nav}</nav>\n'
        f'    <p>{note}</p>\n'
        '  </div>\n'
        '</footer>\n'
    )


DEFAULT_NOTE = "Sandcrab is open-source sandboxing for AI agents, licensed Apache-2.0."
BLOCK = re.compile(r"(<!-- SITE-(HEADER|FOOTER):BEGIN(?: note=\"(.*?)\")? -->\n?)(.*?)(<!-- SITE-\2:END -->)", re.S)


def render(text: str, current: str | None) -> str:
    def sub(m: re.Match) -> str:
        kind, note = m.group(2), m.group(3)
        body = header(current) if kind == "HEADER" else footer(note or DEFAULT_NOTE)
        return f"{m.group(1)}{body}{m.group(5)}"
    return BLOCK.sub(sub, text)


def main() -> int:
    check = "--check" in sys.argv
    stale = []
    for name, current in PAGES.items():
        path = SITE / name
        text = path.read_text()
        if "SITE-HEADER:BEGIN" not in text:
            print(f"skip {name}: no SITE-HEADER markers")
            continue
        out = render(text, current)
        if out != text:
            stale.append(name)
            if not check:
                path.write_text(out)
    if check and stale:
        print("out of date:", ", ".join(stale))
        return 1
    print(("up to date" if check else "updated: " + (", ".join(stale) or "nothing")))
    return 0


if __name__ == "__main__":
    sys.exit(main())
