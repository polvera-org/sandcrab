import { defineConfig } from 'vite';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

// Inlines src/glyphs/<name>.svg wherever the HTML contains <!--glyph:<name>-->.
// The glyph's baked-in colour is dropped so it follows the page theme (currentColor).
function inlineGlyphs() {
  return {
    name: 'inline-glyphs',
    transformIndexHtml(html) {
      return html.replace(/<!--glyph:([\w-]+)-->/g, (_, name) => {
        let svg = readFileSync(resolve('src/glyphs', `${name}.svg`), 'utf8');
        svg = svg
          .replace(/\sstyle="[^"]*"/, '')
          .replace(/\swidth="64"\s+height="64"/, '')
          .replace(/\srole="img"\s+aria-label="[^"]*"/, ' aria-hidden="true" focusable="false"')
          .replace(/<title>[\s\S]*?<\/title>\s*/, '')
          .replace(/<svg /, '<svg class="glyph-svg" ');
        return svg.trim();
      });
    },
  };
}

export default defineConfig({
  base: './', // relative paths: the build can be served from any folder
  plugins: [inlineGlyphs()],
  build: { target: 'es2022', assetsInlineLimit: 0, chunkSizeWarningLimit: 900 },
  server: { host: '0.0.0.0', port: 5173, strictPort: true },
  preview: { host: '0.0.0.0', port: 5173, strictPort: true },
});
