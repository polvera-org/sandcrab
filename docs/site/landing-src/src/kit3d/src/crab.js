// The Sandcrab mascot: low-poly coral crab, articulated legs + claws, toon shaded,
// ink outlined. Faces +Z, walks sideways along ±X (like a real crab).
// All animation is a closed-form function of t (periods 0.5 / 1 / 2 s) so it loops
// seamlessly inside any scene whose loop length is a multiple of 2 s.
import { THREE, createKit, PALETTE as C, TAU, fadeMaterials } from './kit.js';

const BODY_Y = 0.7;
const HIP_Z = 0.5, KNEE_Z = -1.75;

const SHELLS = {
  coral: { body: C.coral, limb: C.coralDeep },
  husk: { body: C.husk, limb: '#D3DCD9' },
  grey: { body: '#93ADB1', limb: '#7F9A9F' },
};

/**
 * @param {object} [opts]
 * @param {number} [opts.scale=1]
 * @param {'idle'|'wave'|'work'|'hold'} [opts.pose='idle']
 * @param {'coral'|'husk'|'grey'} [opts.shell='coral']
 * @param {boolean} [opts.split=false]  shell built from two halves (for molting)
 * @param {'high'|'low'} [opts.detail]  mesh density; defaults to 'low' when scale < 0.3
 * @param {number} [opts.phase=0]       time offset so crowds don't move in sync
 * @returns {{object3d: THREE.Group, update(t:number, dt:number):void, dispose():void,
 *   setPose(p:string):void, setWalk(w:number):void, setOpacity(a:number):void,
 *   setTint(hex:string, k:number):void, parts:object}}
 */
export function createCrab(opts = {}) {
  const { scale = 1, shell = 'coral', split = false, phase = 0 } = opts;
  // detail: 'high' ≈ 1.8k tris, 'low' ≈ 1k tris (auto for small crabs)
  const lo = (opts.detail || (scale < 0.3 ? 'low' : 'high')) === 'low';
  const D = lo
    ? { body: [14, 8], eye: [7, 5], pupil: [5, 4], cap: [1, 4], palm: [7, 5], jaw: [6, 4] }
    : { body: [18, 10], eye: [9, 6], pupil: [6, 4], cap: [1, 6], palm: [9, 7], jaw: [8, 5] };
  let pose = opts.pose || 'idle';
  let walk = 0;
  const k = createKit();
  const pal = SHELLS[shell] || SHELLS.coral;
  const baseBody = new THREE.Color(pal.body);
  const baseLimb = new THREE.Color(pal.limb);
  const mBody = k.toon(pal.body);
  const mLimb = k.toon(pal.limb);
  const mEye = k.toon(C.paper);
  const mPupil = k.basic(C.ink);
  const mOut = k.outline();
  const mats = [mBody, mLimb, mEye, mPupil, mOut];
  const part = (geo, mat, outline = true) => k.mesh(geo, mat, { outline, outlineMat: mOut });

  const root = new THREE.Group();
  root.scale.setScalar(scale);
  const body = new THREE.Group();
  body.position.y = BODY_Y;
  root.add(body);

  // shell: squashed dome (optionally two halves split along the back seam)
  const shellG = new THREE.Group();
  shellG.scale.set(1.15, 0.62, 0.95);
  body.add(shellG);
  const halves = [];
  if (split) {
    for (const s of [-1, 1]) {
      const pivot = new THREE.Group();
      // hinge along the outer lower edge so each half tips outward like a clam
      pivot.position.set(s, -0.2, 0);
      const geo = new THREE.SphereGeometry(1, D.body[0], D.body[1], s < 0 ? -Math.PI / 2 : Math.PI / 2, Math.PI);
      const m = part(geo, mBody);
      m.position.set(-s, 0.2, 0);
      pivot.add(m);
      shellG.add(pivot);
      halves.push({ pivot, s });
    }
    mBody.side = THREE.DoubleSide;
  } else {
    shellG.add(part(new THREE.SphereGeometry(1, ...D.body), mBody));
  }

  // eye stalks
  const eyes = new THREE.Group();
  body.add(eyes);
  for (const s of [-1, 1]) {
    const stalk = part(new THREE.CylinderGeometry(0.045, 0.055, 0.34, 6), mLimb);
    stalk.position.set(s * 0.3, 0.5, 0.6);
    stalk.rotation.x = 0.25;
    const ball = part(new THREE.SphereGeometry(0.13, ...D.eye), mEye);
    ball.position.set(s * 0.3, 0.71, 0.66);
    const pupil = part(new THREE.SphereGeometry(0.065, ...D.pupil), mPupil, false);
    pupil.visible = shell !== 'husk'; // an empty husk has no one looking out
    pupil.position.set(s * 0.3, 0.73, 0.77);
    eyes.add(stalk, ball, pupil);
  }

  // legs: 3 per side; left side is the mirrored right side
  const legs = [];
  const upperGeo = k.geo(new THREE.CapsuleGeometry(0.095, 0.3, ...D.cap).rotateZ(-Math.PI / 2).translate(0.2, 0, 0));
  const lowerGeo = k.geo(new THREE.CapsuleGeometry(0.08, 0.56, ...D.cap).rotateZ(-Math.PI / 2).translate(0.33, 0, 0));
  const sides = {};
  for (const s of [1, -1]) {
    const side = new THREE.Group();
    side.scale.x = s;
    body.add(side);
    sides[s] = side;
    [0.42, 0.02, -0.38].forEach((z, i) => {
      const hip = new THREE.Group();
      hip.position.set(0.9 * Math.sqrt(1 - (z / 0.98) ** 2), -0.2, z);
      hip.rotation.order = 'YZX';
      const yaw0 = [-0.55, -0.05, 0.5][i];
      hip.rotation.y = yaw0;
      hip.rotation.z = HIP_Z;
      const upper = part(upperGeo, mLimb);
      const knee = new THREE.Group();
      knee.position.x = 0.4;
      knee.rotation.z = KNEE_Z;
      knee.add(part(lowerGeo, mLimb));
      hip.add(upper, knee);
      side.add(hip);
      // alternating tripod gait
      legs.push({ hip, knee, yaw0, ph: ((i % 2) ^ (s > 0 ? 0 : 1)) ? Math.PI : 0, i, s });
    });
  }

  // claws
  const claws = {};
  for (const s of [1, -1]) {
    const shoulder = new THREE.Group();
    shoulder.rotation.order = 'YXZ';
    shoulder.position.set(0.72, -0.08, 0.6);
    const arm = part(new THREE.CapsuleGeometry(0.085, 0.36, ...D.cap).rotateX(Math.PI / 2).translate(0, 0, 0.24), mLimb);
    const wrist = new THREE.Group();
    wrist.position.z = 0.5;
    wrist.rotation.y = -0.85;
    const palm = part(new THREE.SphereGeometry(0.3, ...D.palm), mBody);
    palm.scale.set(0.88, 0.78, 1.0);
    palm.position.z = 0.16;
    const jawTop = new THREE.Group();
    jawTop.position.set(0, 0.07, 0.3);
    const top = part(new THREE.SphereGeometry(0.22, ...D.jaw), mBody);
    top.scale.set(0.7, 0.55, 1.5);
    top.position.z = 0.2;
    jawTop.add(top);
    const jawBot = new THREE.Group();
    jawBot.position.set(0, -0.08, 0.28);
    const bot = part(new THREE.SphereGeometry(0.17, ...D.jaw), mBody);
    bot.scale.set(0.66, 0.5, 1.4);
    bot.position.z = 0.15;
    jawBot.add(bot);
    wrist.add(palm, jawTop, jawBot);
    shoulder.add(arm, wrist);
    sides[s].add(shoulder);
    claws[s] = { shoulder, wrist, jawTop, jawBot };
  }

  function posePinch(c, open) { c.jawTop.rotation.x = -open; c.jawBot.rotation.x = open * 0.6; }

  function update(t) {
    const tt = t + phase;
    const w = walk;
    const breathe = Math.sin(TAU * tt / 2);
    body.position.y = BODY_Y + 0.015 * breathe * (1 - w) + 0.03 * w * Math.abs(Math.sin(TAU * tt / 0.5));
    body.rotation.z = 0.04 * w * Math.sin(TAU * tt / 0.5);
    eyes.rotation.z = 0.04 * Math.sin(TAU * tt / 2 + 1);
    eyes.rotation.x = 0.03 * Math.sin(TAU * tt / 1);

    for (const L of legs) {
      const g = TAU * tt / 0.5 + L.ph;
      const idle = 0.03 * Math.sin(TAU * tt / 2 + L.i * 0.9 + L.s);
      L.hip.rotation.z = HIP_Z + idle + w * 0.28 * Math.max(0, Math.sin(g));
      L.hip.rotation.y = L.yaw0 + w * 0.22 * Math.cos(g);
      L.knee.rotation.z = KNEE_Z - w * 0.12 * Math.max(0, Math.sin(g));
    }

    for (const s of [1, -1]) {
      const c = claws[s];
      const sh = c.shoulder;
      sh.rotation.y = 0.55; sh.rotation.x = 0.35; sh.rotation.z = 0;
      c.wrist.rotation.x = 0; c.wrist.rotation.y = -0.85;
      let open = 0.28 + 0.2 * Math.max(0, Math.sin(TAU * tt / 2 + (s > 0 ? 0 : 2.2))) ** 3;
      if (pose === 'wave' && s > 0) {
        sh.rotation.x = -0.7; sh.rotation.y = 1.55;
        sh.rotation.z = 0.3 * Math.sin(TAU * tt / 1);
        c.wrist.rotation.y = 0.1; c.wrist.rotation.x = -0.4;
        open = 0.35;
      } else if (pose === 'work') {
        sh.rotation.x = 0.3 + 0.16 * Math.sin(TAU * tt / 1 + (s > 0 ? 0 : Math.PI));
        open = 0.15 + 0.35 * Math.max(0, Math.sin(TAU * tt / 0.5 + (s > 0 ? 0 : Math.PI)));
      } else if (pose === 'hold' && s > 0) {
        sh.rotation.x = -0.75; sh.rotation.y = 1.5;
        sh.rotation.z = 0.04 * Math.sin(TAU * tt / 2);
        c.wrist.rotation.y = 0.2; c.wrist.rotation.x = -0.5;
        open = 0.32;
      }
      posePinch(c, open);
    }
  }

  return {
    object3d: root,
    update,
    dispose: () => k.dispose(),
    setPose(p) { pose = p; },
    setWalk(v) { walk = Math.min(1, Math.max(0, v)); },
    setOpacity(a) { fadeMaterials(mats.slice(0, 4), a); fadeMaterials([mOut], a ** 4); },
    /** Blend body + limb colour toward hex by k (0..1). */
    setTint(hex, f) {
      const c = new THREE.Color(hex);
      mBody.color.copy(baseBody).lerp(c, f);
      mLimb.color.copy(baseLimb).lerp(c, f);
    },
    /** Open the split shell halves (radians). */
    crack(a) { for (const h of halves) h.pivot.rotation.z = -h.s * a; },
    parts: { body, claws, legs, eyes, shell: shellG },
  };
}

// ── standalone mascot scene ─────────────────────────────────────────────────
export const crabMeta = {
  title: 'Crab',
  fallback: 'molting',
  alt: 'The Sandcrab mascot, a coral crab',
  loop: 12,
  still: 1.2,
  phases: [{ name: 'idle', d: 12 }],
  view: { target: [0, 0.45, 0], radius: 1.75, yaw: 0.5, pitch: 0.52, fraction: 0.74, sway: 0.08 },
};

/** Mascot on a sand disc. opts: pose, shell, scale. */
export function createCrabScene(opts = {}) {
  const k = createKit();
  const g = new THREE.Group();
  g.add(k.disc(2.0, 0.25));
  const crab = createCrab({ pose: opts.pose || 'wave', shell: opts.shell, scale: opts.scale || 1 });
  g.add(crab.object3d);
  return {
    object3d: g,
    meta: crabMeta,
    crab,
    update(t, dt) { crab.update(t, dt); },
    dispose() { crab.dispose(); k.dispose(); },
  };
}
