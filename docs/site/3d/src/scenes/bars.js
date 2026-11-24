// Bars = the canonical benchmark, in 3D. Rows: spawn → ready, destroy, agent turn
// (p50, with a p95 whisker). Columns: Docker (measured, solid teal) and
// Firecracker, Cloud Hypervisor, libkrun (not measured: dashed ghost slots that
// reach the top of the wall, so no height is implied).
// One idea: "Docker is measured; the jars are still TODO."
// Phases: bars grow once (0.3–2.6 s, staggered), then hold. No loop motion besides sway.
// Motion budget: growth on entry only, slow sway.
import { THREE, createKit, PALETTE as C, ease, clamp01, canvasTexture } from '../kit.js';
import { anchor, label, ghostBox } from './common.js';

export const barsMeta = {
  title: 'Bars',
  fallback: 'few-grains',
  alt: '3D bar chart of Docker benchmark medians, with dashed placeholder slots for Firecracker, Cloud Hypervisor and libkrun',
  loop: 12,
  still: 6,
  phases: [{ name: 'grow', d: 2.6 }, { name: 'hold', d: 9.4 }],
  view: { target: [0.1, 0.95, 0], radius: 3.0, yaw: 0.42, pitch: 0.36, fraction: 0.9, sway: 0.03 },
};

const H_MAX = 2.3;   // world height of the wall
const MS_MAX = 500;  // value at the top of the wall

/** opts.data = window.SANDCRAB_DATA */
export function createBars(opts = {}) {
  const b = opts.data?.bench;
  const rows = [
    ['spawn → ready', b?.base_w0?.spawn_ready ?? { p50_ms: 227.42, p95_ms: 347.57 }],
    ['destroy', b?.base_w0?.teardown ?? { p50_ms: 98.29, p95_ms: 187.9 }],
    ['agent turn', b?.agent_turn_base_w1 ?? { p50_ms: 390.02, p95_ms: 456.25 }],
  ];
  const cols = ['Docker', 'Firecracker', 'Cloud Hypervisor', 'libkrun'];
  const k = createKit();
  const g = new THREE.Group();
  const W = 6.2, D = 3.0;
  g.add(k.slab(W, D, 0.3));

  // back wall with gridlines every 100 ms, numbers drawn into the texture
  const wallTex = k.tex(canvasTexture(1024, (ctx, s) => {
    ctx.fillStyle = C.sand50; ctx.fillRect(0, 0, s, s);
    ctx.strokeStyle = C.sandDeep; ctx.lineWidth = 3;
    ctx.fillStyle = C.ink; ctx.font = '600 30px "JetBrains Mono", ui-monospace, monospace'; ctx.textBaseline = 'middle';
    for (let ms = 100; ms <= MS_MAX; ms += 100) {
      const y = s - (ms / MS_MAX) * s;
      ctx.beginPath(); ctx.moveTo(110, y); ctx.lineTo(s - 20, y); ctx.stroke();
      ctx.fillText(`${ms}`, 18, y + (ms === MS_MAX ? 22 : 0));
    }
    ctx.fillText('ms', 18, s - 30);
  }));
  const wall = k.mesh(new THREE.BoxGeometry(W, H_MAX, 0.12).translate(0, H_MAX / 2, 0),
    [k.toon(C.sand200), k.toon(C.sand200), k.toon(C.sand200), k.toon(C.sand200), k.toon('#FFFFFF', { map: wallTex }), k.toon(C.sand200)], { receive: true });
  wall.position.z = -D / 2 + 0.06;
  g.add(wall);

  // one group per backend along X; inside a group, one bar per metric
  const GX = [-2.2, -0.55, 1.05, 2.55];
  const BX = [-0.46, 0, 0.46];
  const Z = -0.2;
  const bars = [];
  const mBar = [k.toon(C.teal500), k.toon(C.teal700), k.toon(C.teal500)];
  const mWhisk = k.toon(C.ink);
  rows.forEach(([name, st], j) => {
    const h = (st.p50_ms / MS_MAX) * H_MAX;
    const bar = k.mesh(new THREE.BoxGeometry(0.38, 1, 0.5).translate(0, 0.5, 0), mBar[j]);
    bar.position.set(GX[0] + BX[j], 0, Z);
    bar.scale.y = 0.0001;
    bar.visible = false;
    g.add(bar);
    const h95 = (st.p95_ms / MS_MAX) * H_MAX;
    const whisk = new THREE.Group();
    const stem = k.mesh(new THREE.BoxGeometry(0.025, 1, 0.025).translate(0, 0.5, 0), mWhisk, { outline: false });
    stem.scale.y = Math.max(0.001, h95 - h);
    stem.position.y = h;
    const cap = k.mesh(new THREE.BoxGeometry(0.16, 0.025, 0.025), mWhisk, { outline: false });
    cap.position.y = h95;
    whisk.add(stem, cap);
    whisk.position.set(GX[0] + BX[j] + 0.1, 0, Z - 0.1);
    g.add(whisk);
    bars.push({ bar, whisk, h, delay: 0.3 + j * 0.45 });
  });
  // one dashed slot per unmeasured backend, full wall height (no value implied)
  for (let i = 1; i < cols.length; i++) {
    const gh = ghostBox(k, 1.3, H_MAX * 0.97, 0.5, { opacity: 0.55, fillOpacity: 0.14 });
    gh.position.set(GX[i], 0, Z);
    g.add(gh);
  }

  const fmt = (v) => `${Math.round(v)} ms`;
  const labels = [
    ...rows.map(([name, st], j) => label(`${name} · ${fmt(st.p50_ms)}`, anchor(g, GX[0] + BX[j], 0.1, Z), { a: 0, cls: 'bar' })),
    label('Docker · measured', anchor(g, GX[0], 0, Z + 0.5), { cls: 'quiet', below: true }),
    label('Firecracker · TODO, needs KVM', anchor(g, GX[1], 0, Z + 0.5), { cls: 'quiet ghost', below: true }),
    label('Cloud Hypervisor · TODO', anchor(g, GX[2], 0, Z + 0.5), { cls: 'quiet ghost', below: true }),
    label('libkrun · TODO', anchor(g, GX[3], 0, Z + 0.5), { cls: 'quiet ghost', below: true }),
  ];
  const tops = rows.map((_, j) => labels[j].at);

  function update(t) {
    bars.forEach((o, j) => {
      const p = ease(clamp01((t - o.delay) / 1.4));
      o.bar.scale.y = Math.max(0.0001, o.h * p);
      o.bar.visible = o.h * p > 0.004; // a flat bar would lie in the slab top plane
      o.whisk.visible = p > 0.98;
      tops[j].position.y = o.h * p + 0.12;
      labels[j].a = clamp01((t - o.delay - 1.1) / 0.4);
    });
  }

  return { object3d: g, meta: barsMeta, labels, update, dispose() { k.dispose(); } };
}
