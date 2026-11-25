// Claws & toolbelt = native tools inside crabd. The crab wears a belt of tool
// cards; its raised claw holds one tool at a time and flips to the next.
// One idea: "a small, sharp toolset, used one at a time."
// Phases (18 s): six tools × (flip 1 → use 0.8 → hold 1.2).
// Motion budget: card flip, one ripple on use, crab idle, slow sway.
import { THREE, createKit, PALETTE as C, ease, clamp01, loopLength, canvasTexture, TAU } from './kit.js';
import { createCrab } from './crab.js';

/** Tool glyphs drawn on canvas (no text beyond terminal glyph). */
export const TOOLS = ['bash', 'file_read', 'file_edit', 'grep', 'glob', 'list_dir'];
const SLOT = 3;
const PHASES = TOOLS.map((name) => ({ name, d: SLOT }));

export const toolbeltMeta = {
  title: 'Toolbelt',
  fallback: 'toolbelt',
  alt: 'A crab wearing a toolbelt, holding up one tool card at a time',
  loop: loopLength(PHASES),
  still: 2.4,
  phases: PHASES,
  view: { target: [0.25, 0.85, 0], radius: 1.9, yaw: 0.4, pitch: 0.42, fraction: 0.74, sway: 0.06 },
};

function drawGlyph(ctx, s, tool) {
  const ink = C.ink, teal = C.teal500;
  ctx.fillStyle = C.paper;
  ctx.fillRect(0, 0, s, s);
  ctx.lineWidth = s * 0.06;
  ctx.lineCap = ctx.lineJoin = 'round';
  ctx.strokeStyle = ink;
  ctx.fillStyle = ink;
  const u = s / 16;
  ctx.save();
  switch (tool) {
    case 'bash': // terminal ">_"
      ctx.fillStyle = C.teal700; ctx.fillRect(2 * u, 3 * u, 12 * u, 10 * u);
      ctx.strokeStyle = C.paper; ctx.beginPath(); ctx.moveTo(4.5 * u, 6 * u); ctx.lineTo(7 * u, 8 * u); ctx.lineTo(4.5 * u, 10 * u); ctx.stroke();
      ctx.beginPath(); ctx.moveTo(8 * u, 10.5 * u); ctx.lineTo(11.5 * u, 10.5 * u); ctx.stroke();
      break;
    case 'file_read': // page with lines
      ctx.strokeRect(4 * u, 2.5 * u, 8 * u, 11 * u);
      ctx.lineWidth = s * 0.045; ctx.strokeStyle = teal;
      for (let i = 0; i < 4; i++) { ctx.beginPath(); ctx.moveTo(5.8 * u, (5.5 + i * 2) * u); ctx.lineTo(10.2 * u, (5.5 + i * 2) * u); ctx.stroke(); }
      break;
    case 'file_edit': // pencil over a line
      ctx.strokeStyle = teal; ctx.beginPath(); ctx.moveTo(3 * u, 13 * u); ctx.lineTo(13 * u, 13 * u); ctx.stroke();
      ctx.strokeStyle = ink; ctx.translate(8 * u, 7 * u); ctx.rotate(-Math.PI / 4);
      ctx.strokeRect(-5 * u, -1.3 * u, 9 * u, 2.6 * u);
      ctx.beginPath(); ctx.moveTo(-5 * u, -1.3 * u); ctx.lineTo(-7.2 * u, 0); ctx.lineTo(-5 * u, 1.3 * u); ctx.stroke();
      break;
    case 'grep': // magnifier
      ctx.beginPath(); ctx.arc(7 * u, 7 * u, 3.8 * u, 0, TAU); ctx.stroke();
      ctx.lineWidth = s * 0.09; ctx.beginPath(); ctx.moveTo(9.8 * u, 9.8 * u); ctx.lineTo(13 * u, 13 * u); ctx.stroke();
      ctx.fillStyle = teal; ctx.globalAlpha = 0.35; ctx.beginPath(); ctx.arc(7 * u, 7 * u, 3 * u, 0, TAU); ctx.fill();
      break;
    case 'glob': // asterisk
      ctx.lineWidth = s * 0.075;
      for (let i = 0; i < 3; i++) {
        const a = (i / 3) * Math.PI + Math.PI / 2;
        ctx.beginPath(); ctx.moveTo(8 * u + Math.cos(a) * 5 * u, 8 * u + Math.sin(a) * 5 * u); ctx.lineTo(8 * u - Math.cos(a) * 5 * u, 8 * u - Math.sin(a) * 5 * u); ctx.stroke();
      }
      break;
    case 'list_dir': // folder
      ctx.fillStyle = teal;
      ctx.beginPath(); ctx.moveTo(2.5 * u, 4.5 * u); ctx.lineTo(6.5 * u, 4.5 * u); ctx.lineTo(7.8 * u, 6 * u); ctx.lineTo(13.5 * u, 6 * u); ctx.lineTo(13.5 * u, 12.5 * u); ctx.lineTo(2.5 * u, 12.5 * u); ctx.closePath(); ctx.fill(); ctx.stroke();
      break;
    default: break;
  }
  ctx.restore();
  ctx.lineWidth = s * 0.05; ctx.strokeStyle = ink; ctx.strokeRect(s * 0.025, s * 0.025, s * 0.95, s * 0.95);
}

export function createToolbelt(opts = {}) {
  const k = createKit();
  const g = new THREE.Group();
  g.add(k.disc(1.95, 0.25));
  const crab = createCrab({ scale: 0.95, pose: 'hold' });
  g.add(crab.object3d);

  const texs = TOOLS.map((tool) => k.tex(canvasTexture(128, (ctx, s) => drawGlyph(ctx, s, tool))));

  // belt: teal band around the shell equator with small holstered cards
  const body = crab.parts.body;
  const belt = k.mesh(new THREE.TorusGeometry(1, 0.075, 6, 48).rotateX(Math.PI / 2), k.toon(C.teal700));
  belt.scale.set(1.13, 1, 0.93);
  belt.position.y = -0.06;
  body.add(belt);
  const mini = [];
  const miniGeo = new THREE.BoxGeometry(0.2, 0.2, 0.03);
  TOOLS.forEach((tool, i) => {
    const a = -0.95 + (i / (TOOLS.length - 1)) * 1.9; // front arc
    const m = k.mesh(miniGeo, [k.toon(C.paper), k.toon(C.paper), k.toon(C.paper), k.toon(C.paper), k.toon('#FFFFFF', { map: texs[i] }), k.toon(C.paper)]);
    m.position.set(Math.sin(a) * 1.12 * 1.02, -0.06, Math.cos(a) * 0.93 * 1.04);
    m.rotation.y = a;
    body.add(m);
    mini.push(m);
  });

  // held card in the raised right claw
  const holder = new THREE.Group();
  holder.position.set(1.42, 0.98, 0.72);
  body.add(holder);
  const face = k.toon('#FFFFFF', { map: texs[0] });
  const paper = k.toon(C.paper);
  const card = k.mesh(new THREE.BoxGeometry(0.62, 0.62, 0.05), [paper, paper, paper, paper, face, paper]);
  card.position.y = 0;
  holder.add(card);

  // use ripple: a ring that expands once around the card
  const ring = new THREE.Mesh(k.geo(new THREE.RingGeometry(0.9, 1, 40)), k.basic(C.teal500, { transparent: true, depthWrite: false, side: THREE.DoubleSide }));
  ring.position.z = 0.04;
  card.add(ring);

  // keep the held card upright and facing the viewer whatever the claw does
  const jaw = crab.parts.claws[1].jawTop;
  const tip = new THREE.Vector3();
  const qParent = new THREE.Quaternion(), qWant = new THREE.Quaternion(), eWant = new THREE.Euler(-0.12, 0.5, 0, 'YXZ');

  function update(t, dt) {
    const L = toolbeltMeta.loop;
    const u = ((t % L) + L) % L;
    const i = Math.floor(u / SLOT) % TOOLS.length;
    const p = u - i * SLOT;
    // flip: rotate away (0→0.5 s), swap face, rotate back (0.5→1 s)
    const f = p < 0.5 ? ease(p / 0.5) : 1 - ease(clamp01((p - 0.5) / 0.5));
    card.rotation.y = f * Math.PI / 2;
    face.map = texs[p < 0.5 ? (i + TOOLS.length - 1) % TOOLS.length : i];
    mini.forEach((m, j) => { m.visible = j !== (p < 0.5 ? (i + TOOLS.length - 1) % TOOLS.length : i); });
    const r = clamp01((p - 1) / 0.8);
    ring.visible = r > 0 && r < 1;
    ring.scale.setScalar(0.35 + 0.35 * ease(r));
    ring.material.opacity = 0.7 * (1 - r);
    crab.update(t, dt);
    g.updateMatrixWorld(true);
    // the card sits just above the raised claw's tip
    jaw.getWorldPosition(tip);
    body.worldToLocal(tip);
    holder.position.set(tip.x + 0.05, tip.y + 0.42, tip.z);
    g.getWorldQuaternion(qWant).multiply(new THREE.Quaternion().setFromEuler(eWant));
    holder.parent.getWorldQuaternion(qParent);
    holder.quaternion.copy(qParent.invert().multiply(qWant));
  }

  return { object3d: g, meta: toolbeltMeta, crab, tools: TOOLS, update, dispose() { crab.dispose(); k.dispose(); } };
}
