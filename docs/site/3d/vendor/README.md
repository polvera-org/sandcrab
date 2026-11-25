# Vendored dependencies

Served from disk so the kit works with the network disabled (no CDN at runtime).

## three.js 0.186.1 (r186), MIT

Source: npm `three@0.186.1`
(tarball sha256 `8cd068708ea44f2c73c944b1cead2ba2f0d5c15c8fc194e5700f4e4f4a033fe7`).
The npm package no longer ships `*.min.js`, so the files below are minified copies
of the upstream ES modules, built with esbuild 0.25.10:

```sh
npx esbuild@0.25.10 build/three.core.js   --minify --format=esm --legal-comments=inline --outfile=three.core.min.js
npx esbuild@0.25.10 build/three.module.js --minify --format=esm --legal-comments=inline --outfile=three.module.min.js
sed -i "s#from\"./three.core.js\"#from\"./three.core.min.js\"#" three.module.min.js
# OrbitControls: bare "three" import rewritten to "../three.module.min.js", then minified
```

| File | Upstream source (sha256) | Vendored file (sha256) |
| --- | --- | --- |
| `three/0.186.1/three.core.min.js` | `build/three.core.js` `9edde002b066a9a05676a6127f67735b62baf399bdea529f2f7e31657da769e6` | `c001a388abbe69583ddd66e9dc18098df55fa81d06bcc5a5722efb6c7f522dc8` |
| `three/0.186.1/three.module.min.js` | `build/three.module.js` `9052042d676cb0fdc1ddfefe193053f34b7ac0513a616fdac4535d49987812ea` | `f6079427efb0eef1149a44d361ff313279a2d8f5b30c7d42fa42aee1904028c9` |
| `three/0.186.1/addons/OrbitControls.js` | `examples/jsm/controls/OrbitControls.js` `3d79d07ecb686b4e5d93232eedab255331c1beef711e13164eaa1f68655a5f2b` | `fe684e9c52ee1b96a0e5ac73b70becf8e9e785d812aa4b0e5813f7b61a1b98b2` |
| `three/0.186.1/LICENSE` | `LICENSE` | `8b378ebe60e2fe500158cb0ac71cb5e8b7d92953c2abcc63a0eb90499653b5bc` |

Verify: `cd docs/site/3d/vendor && sha256sum three/0.186.1/*.js three/0.186.1/addons/*.js`.
