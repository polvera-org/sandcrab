// Lifeline = one sandbox's life, to scale. A playhead runs along a rail at 12×
// slow motion: spawn → ready (the crab appears), tool calls (the crab works),
// destroy (the crab molts away). Segment lengths are the canonical bench p50s.
// One idea: "a sandbox lives for one turn; spawn and teardown are fractions of a second."
// Phases (12 s at the default data): spawn ≈2.7 → ready 0.4 → tools 2.6 → teardown ≈1.2
//   → hold → reset 1. Motion budget: playhead + growing segment, crab, one flash.
import { THREE, createKit, PALETTE as C, ease, win, clamp01, lerp, timeline } from '../kit.js';
import { createCrab } from '../crab.js';
import { anchor, label, loopU } from './common.js';

const SLOW = 12; // animation seconds per real second
const RAIL = { x0: -2.9, len: 5.8, z: 1.05 };
const TOOLS_LEN = 1.5;

function fmtRange([a, b]) { return `${Math.round(a)}–${Math.round(b)} ms`; }

function phasesFor(spawnMs, tearMs) {
  const P = [
    { name: 'spawn', d: (spawnMs / 1000) * SLOW }, { name: 'ready', d: 0.4 }, { name: 'tools', d: 2.6 },
    { name: 'teardown', d: (tearMs / 1000) * SLOW }, { name: 'hold', d: 0 }, { name: 'reset', d: 1 },
  ];
  const used = P.reduce((s, p) => s + p.d, 0);
  P[4].d = Math.max(2.5, 12 - used);
  return P;
}

export const lifelineMeta = {
  title: 'Lifeline',
  fallback: 'molting',
  alt: 'A timeline of one sandbox: spawn, tool calls, teardown, drawn to scale from benchmark medians',
  loop: 12,
  still: 9.2,
  phases: phasesFor(227, 98),
  view: { target: [0, 0.5, 0.3], radius: 2.85, yaw: 0.32, pitch: 0.52, fraction: 0.86, sway: 0.03 },
};

/** opts.data = window.SANDCRAB_DATA (bench numbers); falls back to the canonical values. */
export function createLifeline(opts = {}) {
  const b = opts.data?.bench;
  const spawnMs = b?.base_w0?.spawn_ready?.p50_ms ?? 227.42;
  const tearMs = b?.base_w0?.teardown?.p50_ms ?? 98.29;
  const firstMs = b?.base_w0?.first_call?.p50_ms ?? 3.59;
  const spawnRange = b?.spawn_ready_range_ms ?? [227, 258];
  const tearRange = b?.teardown_range_ms ?? [96, 104];
  const PHASES = phasesFor(spawnMs, tearMs);
  const L = PHASES.reduce((s, p) => s + p.d, 0);
  const meta = { ...lifelineMeta, loop: L, phases: PHASES, still: L - PHASES[5].d - 0.6 };

  const k = createKit();
  const g = new THREE.Group();
  g.add(k.slab(6.8, 3.2, 0.3));

  // the rail: spawn and teardown to scale (same ms → length unit), tools segment fixed with a break
  const unit = (RAIL.len - TOOLS_LEN - 0.2) / (spawnMs + tearMs);
  const L1 = spawnMs * unit, L3 = tearMs * unit;
  const x1 = RAIL.x0, x2 = x1 + L1 + 0.1, x3 = x2 + TOOLS_LEN + 0.1;
  const rail = k.mesh(new THREE.BoxGeometry(RAIL.len, 0.04, 0.26).translate(RAIL.x0 + RAIL.len / 2, 0.02, RAIL.z), k.toon(C.sandDeep), { outline: false, receive: true });
  g.add(rail);
  const seg = (x, len, color, h = 0.16) => {
    const m = k.mesh(new THREE.BoxGeometry(1, h, 0.22).translate(0.5, h / 2 + 0.04, 0), k.toon(color));
    m.position.set(x, 0, RAIL.z);
    m.scale.x = 0.0001;
    g.add(m);
    return { m, x, len };
  };
  const sSpawn = seg(x1, L1, C.teal500);
  const sTear = seg(x3, L3, C.teal700);
  // tools: six short call ticks + two break slashes (not to scale: the turn lasts as long as the agent needs)
  const ticks = [];
  const mTick = k.toon(C.paper);
  for (let i = 0; i < 6; i++) {
    const tk = k.mesh(new THREE.BoxGeometry(0.1, 0.16, 0.22).translate(0, 0.12, 0), mTick);
    tk.position.set(x2 + 0.18 + i * 0.22, 0, RAIL.z);
    tk.visible = false;
    g.add(tk);
    ticks.push(tk);
  }
  const mSlash = k.toon(C.ink);
  [x2 + TOOLS_LEN - 0.12, x2 + TOOLS_LEN - 0.02].forEach((x) => {
    const s = k.mesh(new THREE.BoxGeometry(0.03, 0.26, 0.24), mSlash, { outline: false });
    s.position.set(x, 0.15, RAIL.z);
    s.rotation.z = -0.5;
    g.add(s);
  });
  // playhead
  const head = k.mesh(new THREE.BoxGeometry(0.07, 0.42, 0.07).translate(0, 0.21, 0), k.toon(C.ink), { outline: false });
  const headTop = k.mesh(new THREE.BoxGeometry(0.16, 0.16, 0.16), k.toon(C.teal500, { emissive: new THREE.Color(C.teal500), emissiveIntensity: 0.5 }));
  headTop.position.y = 0.48;
  head.add(headTop);
  head.position.z = RAIL.z;
  g.add(head);

  // the sandbox: a crab that appears on ready, works during tools, molts on teardown
  const crab = createCrab({ scale: 0.55, pose: 'idle' });
  crab.object3d.position.set(0, 0, -0.35);
  g.add(crab.object3d);
  const ring = new THREE.Mesh(k.geo(new THREE.RingGeometry(0.9, 1, 40).rotateX(-Math.PI / 2)), k.basic(C.teal500, { transparent: true, depthWrite: false }));
  ring.position.set(0, 0.02, -0.35);
  g.add(ring);
  const card = k.mesh(new THREE.BoxGeometry(0.22, 0.16, 0.03), k.toon(C.paper));
  g.add(card);

  const aSpawn = anchor(g, x1 + L1 / 2, 0.3, RAIL.z);
  const aTools = anchor(g, x2 + TOOLS_LEN / 2, 0.1, RAIL.z + 0.2);
  const aTear = anchor(g, x3 + L3 / 2, 0.3, RAIL.z);
  const aSlow = anchor(g, 0, 1.45, -0.35);
  const labels = [
    label(`spawn → ready · p50 ${fmtRange(spawnRange)}`, aSpawn, { a: 0 }),
    label(`tool calls · first ${firstMs.toFixed(1)} ms`, aTools, { a: 0, below: true }),
    label(`destroy · p50 ${fmtRange(tearRange)}`, aTear, { a: 0 }),
    label(`${SLOW}× slow motion`, aSlow, { a: 0.75, cls: 'quiet' }),
  ];
  const at = (name) => { let a = 0; for (const p of PHASES) { if (p.name === name) return a; a += p.d; } return a; };
  const T = { spawn: at('spawn'), ready: at('ready'), tools: at('tools'), tear: at('teardown'), hold: at('hold'), reset: at('reset') };

  function update(t, dt) {
    const u = loopU(t, L);
    const tl = timeline(PHASES, u);
    const resetFade = tl.phase === 'reset' ? 1 - ease(tl.p) : 1;
    // segments grow with the playhead (linear in time: it is a clock)
    const pS = clamp01((u - T.spawn) / PHASES[0].d);
    const pT = clamp01((u - T.tools) / PHASES[2].d);
    const pD = clamp01((u - T.tear) / PHASES[3].d);
    sSpawn.m.scale.x = Math.max(0.0001, L1 * pS * resetFade);
    sTear.m.scale.x = Math.max(0.0001, L3 * pD * resetFade);
    ticks.forEach((tk, i) => { tk.visible = pT * 6 > i + 0.3 && resetFade > 0.5; });
    let hx = x1;
    if (u < T.ready) hx = x1 + L1 * pS;
    else if (u < T.tools) hx = x1 + L1;
    else if (u < T.tear) hx = lerp(x2, x2 + TOOLS_LEN, pT);
    else hx = x3 + L3 * pD;
    if (tl.phase === 'reset') hx = lerp(x3 + L3, x1, ease(tl.p));
    head.position.x = hx;

    // crab: hidden while spawning, pops in on ready, works, then tints + fades on teardown
    let sc = 0, fade = 1, tint = 0;
    if (u < T.ready) sc = 0;
    else if (u < T.tools) sc = ease(clamp01((u - T.ready) / 0.4));
    else if (u < T.tear) sc = 1;
    else if (u < T.hold) { sc = 1; tint = clamp01(pD * 1.6); fade = 1 - 0.7 * clamp01((pD - 0.3) / 0.7); }
    else if (u < T.reset) { sc = 1; tint = 1; fade = 0.3; } // the empty husk lingers
    else { sc = 1; tint = 1; fade = 0.3 * (1 - ease(tl.p)); }
    crab.object3d.visible = sc > 0.01 && fade > 0.01;
    crab.object3d.scale.setScalar(0.55 * Math.max(0.001, sc));
    crab.setPose(tl.phase === 'tools' ? 'work' : 'idle');
    crab.setTint(C.husk, tint);
    crab.setOpacity(fade);
    crab.update(t, dt);
    const rp = tl.phase === 'ready' ? tl.p : -1;
    ring.visible = rp >= 0;
    ring.scale.setScalar(0.3 + 0.6 * ease(Math.max(0, rp)));
    ring.material.opacity = 0.7 * (1 - Math.max(0, rp));
    // one tool card per tick, claw → rail
    const cp = tl.phase === 'tools' ? (tl.p * 6) % 1 : -1;
    card.visible = cp >= 0 && cp < 0.8;
    if (card.visible) {
      const e = ease(cp / 0.8);
      const tx = x2 + 0.18 + Math.min(5, Math.floor(tl.p * 6)) * 0.22;
      card.position.set(lerp(0.3, tx, e), 0.55 + Math.sin(Math.PI * e) * 0.35 - 0.3 * e, lerp(-0.1, RAIL.z, e));
    }

    labels[0].a = clamp01((u - T.ready + 0.2) / 0.5) * resetFade;
    labels[1].a = clamp01((u - T.tools - 0.3) / 0.5) * resetFade;
    labels[2].a = clamp01((u - T.hold + 0.1) / 0.4) * resetFade;
  }

  return { object3d: g, meta, labels, update, dispose() { crab.dispose(); k.dispose(); } };
}
