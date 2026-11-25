// Screen = crabagent built the landing page from inside Sandcrab. A monitor shows
// the page it wrote; the crab works in front of it and tool-call cards fly from its
// claws to the screen while a counter climbs to the live-check total.
// One idea: "an agent that lives in the thing it sells built the thing that sells it."
// Phases: count 0.5–9 s (cards fly, counter climbs), then the crab waves. Plays once.
// Motion budget: one card at a time, counter, crab pose; slow sway.
import { THREE, createKit, PALETTE as C, ease, clamp01, lerp } from '../kit.js';
import { createCrab } from '../crab.js';
import { anchor, label } from './common.js';

export const screenMeta = {
  title: 'Screen',
  fallback: 'handoff',
  alt: 'A crab working in front of a monitor that shows the Sandcrab landing page it built',
  loop: 12,
  still: 10,
  phases: [{ name: 'count', d: 9 }, { name: 'done', d: 3 }],
  view: { target: [0.25, 1.0, 0], radius: 2.75, yaw: 0.42, pitch: 0.3, fraction: 0.9, sway: 0.03 },
};

/** opts.data = window.SANDCRAB_DATA; opts.screenshot = URL of the landing screenshot. */
export function createScreen(opts = {}) {
  const live = opts.data?.live ?? { tool_calls: 188, wall_min: 36, wall_s: 36, cost_usd: 4.51, turns: 3, tool_errors: 0 };
  const k = createKit();
  const g = new THREE.Group();
  g.add(k.slab(6.2, 3.2, 0.3));

  // monitor: bezel, screen (16:10 screenshot), stand
  const SW = 3.3, SH = SW * 0.625;
  const mon = new THREE.Group();
  mon.position.set(0.55, 0, -0.55);
  mon.rotation.y = -0.12;
  g.add(mon);
  const bezel = k.mesh(new THREE.BoxGeometry(SW + 0.16, SH + 0.16, 0.1).translate(0, SH / 2 + 0.5, 0), k.toon(C.ink));
  mon.add(bezel);
  const screenMat = k.basic(C.paper);
  const screen = new THREE.Mesh(k.geo(new THREE.PlaneGeometry(SW, SH)), screenMat);
  screen.position.set(0, SH / 2 + 0.5, 0.051);
  mon.add(screen);
  const neck = k.mesh(new THREE.BoxGeometry(0.18, 0.5, 0.1).translate(0, 0.25, 0), k.toon(C.teal700));
  neck.position.z = -0.08;
  const foot = k.mesh(new THREE.BoxGeometry(1.0, 0.06, 0.55).translate(0, 0.03, 0), k.toon(C.teal700));
  foot.position.z = -0.05;
  mon.add(neck, foot);
  if (opts.screenshot) {
    new THREE.TextureLoader().load(opts.screenshot, (tex) => {
      tex.colorSpace = THREE.SRGBColorSpace;
      tex.anisotropy = 4;
      k.tex(tex);
      screenMat.map = tex;
      screenMat.color.set('#FFFFFF');
      screenMat.needsUpdate = true;
      opts.onAsset?.();
    }, undefined, () => {});
  }
  // a soft glow plane that pulses on each card arrival
  const glowMat = k.basic(C.teal500, { transparent: true, opacity: 0, depthWrite: false });
  const glow = new THREE.Mesh(k.geo(new THREE.PlaneGeometry(SW, SH)), glowMat);
  glow.position.set(0, SH / 2 + 0.5, 0.055);
  mon.add(glow);

  // the crab at the front-left, turned toward the screen
  const crab = createCrab({ scale: 0.5, pose: 'work' });
  crab.object3d.position.set(-1.95, 0, 0.75);
  crab.object3d.rotation.y = 0.75;
  g.add(crab.object3d);

  // tool-call card
  const card = k.mesh(new THREE.BoxGeometry(0.2, 0.14, 0.03), k.toon(C.paper));
  g.add(card);
  const target = new THREE.Vector3();

  const aCount = anchor(g, -1.95, 1.25, 0.75);
  const labels = [
    label(`0 tool calls`, aCount, { cls: 'stat' }),
    label(`${live.wall_min} min · ${live.turns} turns · $${live.cost_usd.toFixed(2)}`, anchor(g, 0.55, 0.12, 0.6), { cls: 'stat', a: 0 }),
  ];

  const N = live.tool_calls;
  const CARDS = 12; // cards shown; the counter carries the real total
  function update(t, dt) {
    const p = clamp01((t - 0.5) / 8.5);
    const count = Math.round(N * ease(p));
    labels[0].text = `${count} tool calls${p >= 1 ? ` · ${live.tool_errors} errors` : ''}`;
    labels[1].a = clamp01((t - 8.6) / 0.6);
    crab.setPose(p < 1 ? 'work' : 'wave');
    crab.update(t, dt);
    const cp = p < 1 ? (p * CARDS) % 1 : -1;
    card.visible = cp >= 0 && cp < 0.85;
    if (card.visible) {
      const e = ease(cp / 0.85);
      const i = Math.floor(p * CARDS);
      target.set(lerp(-0.9, 1.6, (i * 0.37) % 1), 1.05 + ((i * 0.53) % 1) * 1.2, -0.45);
      card.position.set(lerp(-1.6, target.x, e), lerp(0.75, target.y, e) + Math.sin(Math.PI * e) * 0.4, lerp(0.7, target.z, e));
      card.rotation.set(0, -0.12, e * 0.6);
    }
    const arrive = p < 1 ? Math.max(0, ((p * CARDS) % 1) - 0.85) / 0.15 : 0;
    glowMat.opacity = 0.12 * Math.sin(Math.PI * arrive);
  }

  return { object3d: g, meta: screenMeta, labels, update, dispose() { crab.dispose(); k.dispose(); } };
}
